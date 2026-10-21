# Mock SimplyPrint cloud: records packets per instance path (port 7914)
import json, tornado.ioloop, tornado.web, tornado.websocket
LOG = {}
class WS(tornado.websocket.WebSocketHandler):
    def open(self, inst):
        self.inst = inst
        LOG.setdefault(inst, [])
        self.write_message(json.dumps({"type": "connected", "data": {"interval": {"job": 2000, "temps": 1000}, "name": "MockPrinter"}}))
        self.write_message(json.dumps({"type": "demand", "data": {"demand": "terminal", "enabled": True}}))
        self.write_message(json.dumps({"type": "demand", "data": {"demand": "gcode", "list": ["M117 hello"], "identifier": "abc"}}))
    def on_message(self, msg):
        pkt = json.loads(msg)
        if pkt["type"] == "ping":
            self.write_message(json.dumps({"type": "pong"}))
        LOG[self.inst].append(pkt)
class Dump(tornado.web.RequestHandler):
    def get(self, inst):
        self.write(json.dumps(LOG.get(inst, [])))
app = tornado.web.Application([(r"/ws/(\w+)", WS), (r"/dump/(\w+)", Dump)])
app.listen(7914)
tornado.ioloop.IOLoop.current().start()
