cd "$(dirname "$0")" && timeout 600 ~/mr/venv/bin/python ext_test.py 2>&1 | tail -120
