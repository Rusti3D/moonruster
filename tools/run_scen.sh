cd "$(dirname "$0")" && timeout 1500 ~/mr/venv/bin/python scenario.py 2>&1 | tail -150
