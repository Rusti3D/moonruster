# Mock power devices (tasmota, shelly, generic http) for power component tests (port 7913)
import json, tornado.ioloop, tornado.web
STATE = {"tasmota": "OFF", "shelly": False, "generic": "off"}
class Tasmota(tornado.web.RequestHandler):
    def get(self):
        cmd = self.get_argument("cmnd")
        parts = cmd.split()
        if len(parts) > 1:
            STATE["tasmota"] = parts[-1].upper()
        self.write({"POWER1": STATE["tasmota"]})
class Shelly(tornado.web.RequestHandler):
    def get(self, rid):
        turn = self.get_argument("turn", None)
        if turn is not None:
            STATE["shelly"] = turn == "on"
        self.write({"ison": STATE["shelly"], "timer_remaining": 0})
class Generic(tornado.web.RequestHandler):
    def get(self, action):
        if action in ("on", "off"):
            STATE["generic"] = action
        self.write({"state": STATE["generic"]})
    def post(self, action):
        body = json.loads(self.request.body or b"{}")
        STATE["generic"] = body.get("set", STATE["generic"])
        self.write({"state": STATE["generic"], "hdr": self.request.headers.get("X-Test", "")})
WLED_LOG = []
class Wled(tornado.web.RequestHandler):
    def post(self):
        body = json.loads(self.request.body)
        WLED_LOG.append(body)
        self.write({"state": {"on": True}, "info": {"ver": "0.14"}} if body.get("v") else {"success": True})
    def get(self):
        self.write(json.dumps(WLED_LOG))
        WLED_LOG.clear()
app = tornado.web.Application([(r"/json", Wled), (r"/cm", Tasmota), (r"/relay/(\d+)", Shelly), (r"/generic/(\w+)", Generic)])
app.listen(7913)
tornado.ioloop.IOLoop.current().start()
