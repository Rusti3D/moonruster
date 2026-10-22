# N websocket clients subscribing to all objects, like Mainsail; runs for SECS
import sys, json, asyncio
from tornado.websocket import websocket_connect
port, n, secs = int(sys.argv[1]), int(sys.argv[2]), float(sys.argv[3])
async def client(i, counts):
    ws = await websocket_connect(f"ws://127.0.0.1:{port}/websocket")
    objs = {o: None for o in ["toolhead","extruder","heater_bed","print_stats","virtual_sdcard","display_status","motion_report","gcode_move","fan","webhooks"]}
    ws.write_message(json.dumps({"jsonrpc": "2.0", "method": "printer.objects.subscribe", "params": {"objects": objs}, "id": 1}))
    while True:
        m = await ws.read_message()
        if m is None: return
        if '"notify_status_update"' in m: counts[i] += 1
async def main():
    counts = [0]*n
    tasks = [asyncio.ensure_future(client(i, counts)) for i in range(n)]
    await asyncio.sleep(secs)
    print("status updates received per client:", counts)
asyncio.run(main())
