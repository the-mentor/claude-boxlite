#!/usr/bin/env python3
"""Store an image registry's credentials in registries.local.json (gitignored),
a BoxLite --config file for authenticated registries (e.g. ECR) that stays
out of the repo. Mirrors `docker login`'s interface:

    aws ecr get-login-password --region us-east-1 \\
      | scripts/registry-login.py --registry 123456789012.dkr.ecr.us-east-1.amazonaws.com \\
          --username AWS --password-stdin

The password is read from stdin. If the registry is already present in the
config file, its auth block is updated in place; otherwise a new entry is
appended. A missing or empty config file is treated as {"image_registries": []}.
"""
import argparse
import json
import os
import sys
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "--registry",
        required=True,
        help="registry host, e.g. 123456789012.dkr.ecr.us-east-1.amazonaws.com",
    )
    parser.add_argument("--username", required=True)
    parser.add_argument(
        "--password-stdin",
        action="store_true",
        required=True,
        help="read the password from stdin (the only supported mode)",
    )
    default_config = os.environ.get(
        "REGISTRIES_CONFIG",
        str(Path(__file__).resolve().parent.parent / "registries.local.json"),
    )
    parser.add_argument("--config", default=default_config)
    args = parser.parse_args()

    password = sys.stdin.read().strip()
    if not password:
        sys.exit("registry-login: empty password on stdin")

    config_path = Path(args.config)
    try:
        raw = config_path.read_text().strip()
    except FileNotFoundError:
        raw = ""

    data = json.loads(raw) if raw else {}
    if not isinstance(data, dict):
        sys.exit(f"registry-login: {config_path} does not contain a JSON object")

    registries = data.setdefault("image_registries", [])
    if not isinstance(registries, list):
        sys.exit(f"registry-login: {config_path}'s image_registries is not a list")

    auth = {"type": "basic", "username": args.username, "password": password}
    for entry in registries:
        if isinstance(entry, dict) and entry.get("host") == args.registry:
            entry["auth"] = auth
            break
    else:
        registries.append({"host": args.registry, "transport": "https", "auth": auth})

    config_path.write_text(json.dumps(data, indent=2) + "\n")
    config_path.chmod(0o600)

    print(f"registry-login: wrote credentials for {args.registry} to {config_path}", file=sys.stderr)


if __name__ == "__main__":
    main()
