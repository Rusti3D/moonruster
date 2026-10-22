#!/usr/bin/env python3
# Mock smart plug endpoints for the power differential test.  Binds
# 127.0.0.2:80 (http devices) and 127.0.0.2:9999 (tplink), logs every
# request.  GET /_log returns and clears the log, POST /_reset resets state.
import json
import base64
import struct
import asyncio
import tornado.web
import tornado.ioloop

LOG = []
ST = {}

def reset():
    ST.clear()
    ST.update({"tas": "OFF", "shelly": False, "st": "off", "hs": "Off", "ha": "off",
               "lox": 0, "hue_light": False, "hue_group": False,
               "tp": 0, "tp_children": [0, 0], "tp_rules": []})

reset()

class Base(tornado.web.RequestHandler):
    def log(self):
        auth = self.request.headers.get("Authorization", "")
        if auth.startswith("Basic "):
            auth = "Basic " + base64.b64decode(auth[6:]).decode()
        LOG.append({
            "method": self.request.method,
            "path": self.request.path,
            "query": self.request.query,
            "auth": auth,
            "ctype": self.request.headers.get("Content-Type", ""),
            "body": self.request.body.decode(errors="replace"),
        })

class Ctl(tornado.web.RequestHandler):
    def get(self):
        global LOG
        self.write(json.dumps(LOG))
        LOG = []
    def post(self):
        global LOG
        reset()
        LOG = []
        self.write("ok")

class Tasmota(Base):
    def get(self):
        self.log()
        cmd = self.get_argument("cmnd")
        parts = cmd.split(";")[-1].split()
        if len(parts) > 1:
            ST["tas"] = parts[-1].upper()
        self.write({"POWER1": ST["tas"]})

class Shelly(Base):
    def get(self, rid):
        self.log()
        turn = self.get_argument("turn", None)
        if turn is not None:
            ST["shelly"] = turn == "on"
        timer = self.get_argument("timer", None)
        self.write({"ison": ST["shelly"], "timer_remaining": int(timer) if timer else 0})

class SmartThings(Base):
    def get(self, dev):
        self.log()
        self.write({"switch": {"value": ST["st"].upper()}})
    def post(self, dev):
        self.log()
        body = json.loads(self.request.body)
        ST["st"] = body[0]["command"]
        self.write({"results": [{"id": "x", "status": "ACCEPTED"}]})

class HomeSeer(Base):
    def get(self):
        self.log()
        req = self.get_argument("request")
        if req == "controldevicebylabel":
            ST["hs"] = self.get_argument("label")
            self.write({"Response": "ok"})
        else:
            self.write({"Devices": [{"ref": 42, "status": ST["hs"]}]})

class HAState(Base):
    def get(self, dev):
        self.log()
        self.write({"entity_id": dev, "state": ST["ha"]})

class HAService(Base):
    def post(self, domain, cmd):
        self.log()
        ST["ha"] = "on" if cmd == "turn_on" else "off"
        self.write(json.dumps([]))

class Loxone(Base):
    def get(self, oid, cmd=None):
        self.log()
        if cmd in ("on", "off"):
            ST["lox"] = 1 if cmd == "on" else 0
        self.write({"LL": {"control": f"jdev/sps/io/{oid}", "value": str(ST["lox"]), "Code": "200"}})

class Hue(Base):
    def get(self, user, kind, did):
        self.log()
        if kind == "groups":
            self.write({"action": {"on": ST["hue_group"]}, "state": {"all_on": ST["hue_group"], "any_on": ST["hue_group"]}})
        else:
            self.write({"state": {"on": ST["hue_light"]}})
    def put(self, user, kind, did, key):
        self.log()
        body = json.loads(self.request.body)
        if kind == "groups":
            ST["hue_group"] = body["on"]
        else:
            ST["hue_light"] = body["on"]
        self.write(json.dumps([{"success": {f"/{kind}/{did}/{key}/on": body["on"]}}]))

def tp_decrypt(data):
    key = 0xAB
    out = bytearray()
    for c in data:
        out.append(key ^ c)
        key = c
    return out.decode()

def tp_encrypt(s):
    data = s.encode()
    key = 0xAB
    out = bytearray(struct.pack(">I", len(data)))
    for c in data:
        key = key ^ c
        out.append(key)
    return bytes(out)

async def tplink(reader, writer):
    hdr = await reader.readexactly(4)
    length = struct.unpack(">I", hdr)[0]
    data = await reader.readexactly(length)
    cmd = json.loads(tp_decrypt(data))
    LOG.append({"method": "TPLINK", "path": "", "query": "", "auth": "", "ctype": "", "body": json.dumps(cmd, sort_keys=True)})
    resp = {}
    if "system" in cmd:
        sysc = cmd["system"]
        if "get_sysinfo" in sysc:
            resp = {"system": {"get_sysinfo": {"relay_state": ST["tp"], "children": [
                {"id": "CHILD0", "state": ST["tp_children"][0]}, {"id": "CHILD1", "state": ST["tp_children"][1]}]}}}
        elif "set_relay_state" in sysc:
            st = sysc["set_relay_state"]["state"]
            if "context" in cmd:
                idx = int(cmd["context"]["child_ids"][0][-1])
                ST["tp_children"][idx] = st
            else:
                ST["tp"] = st
            resp = {"system": {"set_relay_state": {"err_code": 0}}}
    elif "count_down" in cmd:
        if "add_rule" in cmd["count_down"]:
            ST["tp"] = 0
            resp = {"count_down": {"add_rule": {"id": "r1", "err_code": 0}}}
        else:
            resp = {"count_down": {"delete_all_rules": {"err_code": 0}}}
    writer.write(tp_encrypt(json.dumps(resp)))
    await writer.drain()
    writer.close()

async def main():
    app = tornado.web.Application([
        (r"/_log", Ctl),
        (r"/cm", Tasmota),
        (r"/relay/(\d+)", Shelly),
        (r"/v1/devices/([^/]+)/commands", SmartThings),
        (r"/v1/devices/([^/]+)/components/main/capabilities/switch/status", SmartThings),
        (r"/JSON", HomeSeer),
        (r"/api/states/([^/]+)", HAState),
        (r"/api/services/([^/]+)/(turn_on|turn_off)", HAService),
        (r"/jdev/sps/io/([^/]+)/(on|off)", Loxone),
        (r"/jdev/sps/io/([^/]+)", Loxone),
        (r"/api/([^/]+)/(lights|groups)/([^/]+)/([^/]+)", Hue),
        (r"/api/([^/]+)/(lights|groups)/([^/]+)", Hue),
    ])
    app.listen(80, address="127.0.0.2")
    await asyncio.start_server(tplink, "127.0.0.2", 9999)
    await asyncio.Event().wait()

asyncio.run(main())
