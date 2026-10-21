# Plays a PanelDue on a pty: sends gcode lines, prints the json responses
import os, sys, time, select, tty
name = sys.argv[1]
master, slave = os.openpty()
tty.setraw(master)
link = f"/tmp/pdue_{name}"
if os.path.lexists(link):
    os.unlink(link)
os.symlink(os.ttyname(slave), link)
def cs(line):
    c = 0
    for ch in line:
        c ^= ord(ch)
    return f"{line}*{c & 0xFF}\n"
lines = [cs("N1 M408 S0"), cs("N2 M408 S1 R0"), cs("N3 M20 S2 P\"0:/gcodes\""), cs("N4 M98 P\"0:/macros/RESTART\""),
         "N5 M408 S0*99\n", cs("N6 M36"), cs("N7 M120"), cs("N8 M408 S0 R5")]
time.sleep(float(sys.argv[2]))
out = b""
for l in lines:
    os.write(master, l.encode())
    end = time.time() + 1.0
    while time.time() < end:
        r, _, _ = select.select([master], [], [], 0.1)
        if r:
            out += os.read(master, 65536)
print(out.decode(errors="replace"))
