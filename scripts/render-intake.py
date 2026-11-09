#!/usr/bin/env python3
"""Write the instance token files and the agent text, from the one table.

Run by `scripts/compose-up.sh` before the collector starts, and by the
acceptance script against synthetic data roots. Usage:

    render-intake.py <table> <telemetry-base> <stack-dir>

Exit 2 on any refusal, with the reason on stderr.
"""

import json
import os
import shutil
import sys
import tempfile


# Named after the program that actually failed: two callers use this file, and
# a refusal reported as `compose-up:` sends the operator to the wrong one.
PROGRAM = os.path.splitext(os.path.basename(sys.argv[0]))[0] or "render-intake"


def fail(message):
    print(f"{PROGRAM}: {message}", file=sys.stderr)
    sys.exit(2)


def credential_for(data_root):
    # tagma resolves its own credential by the relay entry name
    # (credentials/<entry>/tagma.token); a flat credentials/tagma.token only
    # exists before the layout migration has run.
    root = os.path.join(data_root, 'credentials')
    try:
        entries = sorted(os.listdir(root))
    except OSError as error:
        fail(f'cannot list the credentials of {data_root} at {root}: {error}')
    found = [
        os.path.join(root, name, 'tagma.token')
        for name in entries
        if os.path.isfile(os.path.join(root, name, 'tagma.token'))
    ]
    if len(found) != 1:
        seen = ', '.join(found) if found else 'none'
        print(
            f'{PROGRAM}: tagma itself takes the first entry in polis.toml that has'
            ' credentials, and a flat credentials/tagma.token is only what an'
            ' unmigrated layout has: boot that instance once to let it migrate',
            file=sys.stderr,
        )
        fail(f'{data_root} has {len(found)} credentials, expected exactly one at'
             f' credentials/<entry>/tagma.token (found {seen})')
    return found[0]

def render(table_path, telemetry_base, stack_dir):
    try:
        with open(table_path, encoding="utf-8") as handle:
            rows = json.load(handle)
    except (OSError, ValueError) as error:
        fail(f"cannot read {table_path}: {error}")
    if not isinstance(rows, list):
        fail(f"{table_path} must hold a list of instances")
    tokens_root = os.path.realpath(os.path.join(telemetry_base, 'gateway', 'tokens'))
    for row in rows:
        for field in ("slug", "otlpPort", "metricsPort", "dataRoot", "tokenPath"):
            if field not in row:
                fail(f"{table_path}: an instance has no '{field}'")
        token_path = row["tokenPath"]
        resolved = os.path.realpath(token_path)
        if not os.path.isabs(token_path) or not resolved.startswith(tokens_root + os.sep):
            fail(f"{table_path}: {row['slug']} writes its token outside {tokens_root}")
        source = credential_for(row['dataRoot'])
        try:
            with open(source, "rb") as handle:
                raw = handle.read()
        except OSError as error:
            fail(f"cannot read the credential for {row['slug']} at {source}: {error}")
        try:
            token = raw.decode("utf-8").strip()
        except UnicodeDecodeError:
            fail(f"{source} is not UTF-8")
        if not token:
            fail(f"{source} is empty once trimmed")
        if any(ord(char) < 0x20 or ord(char) == 0x7F for char in token):
            fail(f"{source} carries a control character; a stray newline makes every push a 401")
        os.makedirs(os.path.dirname(token_path), exist_ok=True)
        handle, staged = tempfile.mkstemp(dir=os.path.dirname(token_path), prefix=".token-")
        with os.fdopen(handle, "w", encoding="utf-8") as written:
            written.write(token)
        os.chmod(staged, 0o600)
        os.replace(staged, token_path)

    # The agent runs on the host: its text is installed beside the tokens, from
    # the same build, so a port in it and a port in the collector text come from
    # one table.
    agent = os.path.join(stack_dir, "agent.yaml")
    if rows:
        if not os.path.exists(agent):
            fail(f"{stack_dir} has no agent.yaml although {table_path} lists instances")
        target = os.path.join(telemetry_base, "agent.yaml")
        os.makedirs(telemetry_base, exist_ok=True)
        staged = target + ".tmp"
        shutil.copyfile(agent, staged)
        os.chmod(staged, 0o444)
        os.replace(staged, target)


def main(argv):
    if len(argv) != 3:
        print(f"{PROGRAM}: usage: <table> <telemetry-base> <stack-dir>", file=sys.stderr)
        return 2
    render(argv[0], argv[1], argv[2])
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
