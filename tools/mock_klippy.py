#!/usr/bin/env python3
# Fake Klipper API socket: answers moonraker's startup requests, then streams
# status updates at RATE Hz to the process_status_update template.
import socket, json, sys, time, threading, os, math
path, rate = sys.argv[1], float(sys.argv[2])
if os.path.exists(path): os.unlink(path)
srv = socket.socket(socket.AF_UNIX); srv.bind(path); srv.listen(4)
OBJS = ["webhooks","toolhead","extruder","heater_bed","print_stats","virtual_sdcard","display_status","motion_report","gcode_move","fan","idle_timeout","configfile","heaters"]
def status(t):
    s = math.sin(t)
    return {
        "toolhead": {"position": [100+50*s, 100+50*math.cos(t), 10.2, 1234.5+t], "print_time": t, "estimated_print_time": t+0.1},
        "extruder": {"temperature": 215.0+s, "target": 215.0, "power": 0.45+0.1*s, "pressure_advance": 0.04},
        "heater_bed": {"temperature": 60.0+0.1*s, "target": 60.0, "power": 0.3},
        "motion_report": {"live_position": [100+50*s, 100+50*math.cos(t), 10.2, 1234.5+t], "live_velocity": 120.0+s, "live_extruder_velocity": 3.2},
        "print_stats": {"print_duration": t, "total_duration": t+5, "filament_used": 1000+t},
        "virtual_sdcard": {"progress": (t % 1000)/1000, "file_position": int(t*1000)},
        "display_status": {"progress": (t % 1000)/1000},
        "gcode_move": {"gcode_position": [100+50*s, 100, 10.2, 1234.5+t], "speed": 6000.0},
        "fan": {"speed": 1.0, "rpm": None},
    }
def handle(c):
    buf = b""; tmpl = None
    def send(o): c.sendall(json.dumps(o).encode() + b"\x03")
    def streamer():
        t0 = time.time()
        while True:
            time.sleep(1/rate)
            if tmpl:
                try: send({"method": tmpl, "params": {"eventtime": time.time()-t0, "status": status(time.time())}})
                except OSError: return
    threading.Thread(target=streamer, daemon=True).start()
    while True:
        d = c.recv(65536)
        if not d: return
        buf += d
        while b"\x03" in buf:
            m, buf = buf.split(b"\x03", 1); req = json.loads(m)
            meth, p, rid = req.get("method"), req.get("params", {}), req.get("id")
            if meth == "info":
                r = {"state": "ready", "state_message": "Printer is ready", "hostname": "mock", "software_version": "v0.12.0", "cpu_info": "mock", "klipper_path": "/tmp", "python_path": "/usr/bin/python3", "log_file": "/tmp/klippy.log", "config_file": "/tmp/printer.cfg", "app": "Klipper"}
            elif meth == "list_endpoints":
                r = {"endpoints": ["info","objects/list","objects/query","objects/subscribe","gcode/script","gcode/help","gcode/subscribe_output","register_remote_method","emergency_stop","list_endpoints"]}
            elif meth == "objects/list":
                r = {"objects": OBJS}
            elif meth in ("objects/subscribe", "objects/query"):
                if meth == "objects/subscribe":
                    tmpl = p.get("response_template", {}).get("method", tmpl)
                st = status(time.time()); st["webhooks"] = {"state": "ready", "state_message": "Printer is ready"}
                st["configfile"] = {"settings": {}, "config": {}}; st["idle_timeout"] = {"state": "Printing"}; st["heaters"] = {"available_heaters": ["extruder","heater_bed"], "available_sensors": []}
                r = {"eventtime": 1.0, "status": {k: v for k, v in st.items() if k in (p.get("objects") or {})}}
            else:
                r = {}
            if rid is not None: send({"id": rid, "result": r})
while True:
    c, _ = srv.accept(); threading.Thread(target=handle, args=(c,), daemon=True).start()
