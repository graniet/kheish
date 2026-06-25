#!/usr/bin/env python3

import sys

from platforms import CONNECTOR_FACTORIES


def main(argv: list[str]) -> int:
    if len(argv) != 2 or argv[1] not in CONNECTOR_FACTORIES:
        available = ", ".join(sorted(CONNECTOR_FACTORIES))
        sys.stderr.write(
            "usage: python3 connectors/python/run_connector.py <connector>\n"
            f"available: {available}\n"
        )
        return 2
    app = CONNECTOR_FACTORIES[argv[1]]()
    app.start()
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
