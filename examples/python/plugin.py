import json

import extism

DEFAULT_IMAGE = "rust:1.90-alpine"
DEFAULT_ENV = []

# Chain of multiple commands to execute sequentially inside the container.
# Defined directly in the Python source code; the target repository is received dynamically from webhook push events.
COMMANDS = [
    ["cargo", "check"],
    ["cargo", "build"],
]


@extism.import_fn("extism:host/user", "run_container")
def run_container(request: str) -> str:
    pass


@extism.import_fn("extism:host/user", "git_shallow_clone")
def git_shallow_clone(request: str) -> str:
    pass


@extism.plugin_fn
def run():
    raw_input = extism.input_str()
    data = json.loads(raw_input) if raw_input else {}
    config = data.get("pipeline", data) if isinstance(data, dict) else {}

    # 1. Set the git repository:
    # Accept the git repo URL dynamically from the webhook push event or input.
    repository_id = config.get("repository_id")
    if not repository_id:
        git_url = (
            config.get("git_url")
            or data.get("git_url")
            or data.get("github", {})
            .get("payload", {})
            .get("repository", {})
            .get("clone_url")
        )
        if not git_url:
            raise ValueError(
                "no git_url provided: expected repository clone URL from webhook push event or pipeline input"
            )

        clone_response = git_shallow_clone(json.dumps({"url": git_url}))
        clone_result = json.loads(clone_response)
        repository_id = clone_result.get("repository_id")
        if not repository_id:
            raise RuntimeError(
                f"git clone failed or omitted repository_id for {git_url}"
            )

    image = config.get("image", DEFAULT_IMAGE)
    env = config.get("env", DEFAULT_ENV)

    commands_list = COMMANDS

    step_results = []
    chain_success = True
    total_commands = len(commands_list)

    # 2. Run the chain of multiple commands sequentially against the repository.
    for idx, item in enumerate(commands_list):
        if isinstance(item, list):
            cmd = item
            img = image
            e = env
        elif isinstance(item, dict):
            cmd = item.get("command", [])
            img = item.get("image", image)
            e = item.get("env", env)
        else:
            raise ValueError(
                f"invalid command format at index {idx}: expected argv array or object"
            )

        run_req = {
            "image": img,
            "command": cmd,
            "repository_id": repository_id,
            "env": e,
        }

        run_response = run_container(json.dumps(run_req))
        run_result = json.loads(run_response)
        exit_code = run_result.get("exit_code", -1)

        step_record = {
            "step": idx + 1,
            "command": cmd,
            "exit_code": exit_code,
        }
        if "artifact_dir" in run_result:
            step_record["artifact_dir"] = run_result["artifact_dir"]

        step_results.append(step_record)

        # Fail-fast: stop executing the chain if any command fails
        if exit_code != 0:
            chain_success = False
            break

    response = {
        "repository_id": repository_id,
        "success": chain_success,
        "executed_commands": len(step_results),
        "total_commands": total_commands,
        "steps": step_results,
    }

    delivery_id = data.get("github", {}).get("delivery_id")
    if delivery_id:
        response["delivery_id"] = delivery_id

    extism.output_str(json.dumps(response))
