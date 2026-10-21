# Minimal Spoolman stand-in for testing the spoolman component (port 7912)
import json, tornado.ioloop, tornado.web, tornado.websocket
USES = []
class WS(tornado.websocket.WebSocketHandler):
    clients = set()
    def open(self): WS.clients.add(self)
    def on_close(self): WS.clients.discard(self)
class Spool(tornado.web.RequestHandler):
    def get(self, sid):
        if sid == "404":
            self.set_status(404); self.write({"message": "No spool with ID 404 found."}); return
        self.set_header("X-Total-Count", "1")
        self.write({"id": int(sid), "remaining_weight": 900.0})
class Use(tornado.web.RequestHandler):
    def put(self, sid):
        USES.append([int(sid), json.loads(self.request.body)["use_length"]])
        self.write({"id": int(sid)})
class Uses(tornado.web.RequestHandler):
    def get(self): self.write(json.dumps(USES))
class Delete(tornado.web.RequestHandler):
    def get(self, sid):
        for c in WS.clients:
            c.write_message(json.dumps({"type": "deleted", "resource": "spool", "payload": {"id": int(sid)}}))
app = tornado.web.Application([
    (r"/api/v1/spool", WS), (r"/api/v1/spool/(\d+)", Spool), (r"/api/v1/spool/(\d+)/use", Use),
    (r"/uses", Uses), (r"/delete/(\d+)", Delete)])
app.listen(7912)
tornado.ioloop.IOLoop.current().start()
