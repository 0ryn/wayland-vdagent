import os, socket, struct, subprocess, time, pathlib
root=pathlib.Path(__file__).resolve().parents[1]
runtime=os.environ['VDAGENT_TEST_RUNTIME']
env=os.environ|{'XDG_RUNTIME_DIR':runtime,'WAYLAND_DISPLAY':'wayland-0','WAYLAND_VDAGENT_SOCKET':runtime+'/daemon.sock'}
path=env['WAYLAND_VDAGENT_SOCKET']
try: os.unlink(path)
except FileNotFoundError: pass
server=socket.socket(socket.AF_UNIX);server.bind(path);server.listen();server.settimeout(5)
log=open(pathlib.Path(runtime)/'agent-test.log','w')
agent=subprocess.Popen([os.environ.get('VDAGENT_TEST_BINARY', str(root/'target/release/wayland-vdagent'))],env=env,stderr=log)
c,_=server.accept();c.settimeout(5)
def send(t,s=0,k=0,data=b''): c.sendall(struct.pack('<IIII',t,s,k,len(data))+data)
def exact(n):
 b=b''
 while len(b)<n:
  v=c.recv(n-len(b))
  if not v: raise RuntimeError('agent disconnected')
  b+=v
 return b
def read():
 t,s,k,n=struct.unpack('<IIII',exact(16)); return t,s,k,exact(n)
def until(t,sel):
 for _ in range(30):
  m=read()
  if m[:2]==(t,sel): return m
 raise AssertionError((t,sel))
def paste(sel): return subprocess.Popen(['wl-paste','--no-newline','--type','text/plain;charset=utf-8']+(['--primary'] if sel else []),stdout=subprocess.PIPE,stderr=subprocess.PIPE,env=env)
def result(p,want):
 out,err=p.communicate(timeout=5);assert p.returncode==0,(p.returncode,err);assert out==want,(out,want)
def host_grab(sel): send(2,sel,0,struct.pack('<I',1));time.sleep(.1)
try:
 time.sleep(.3)
 host_grab(0);host_grab(1)
 for sel in (0,1,0,1):
  p=paste(sel);m=until(3,sel);assert m[2]==1
  # Interleave a message for the other selection: must survive a pending paste.
  if sel==0: send(2,1,0,struct.pack('<I',1))
  payload=f'host selection {sel} — UTF-8'.encode();send(4,sel,1,payload);result(p,payload)
 print('PASS host -> guest CLIPBOARD and PRIMARY; alternating selections and interleaved GRAB')
 copies=[]
 for sel in (0,1):
  data=f'guest selection {sel}'.encode()
  p=subprocess.Popen(['wl-copy','--foreground','--type','text/plain;charset=utf-8']+(['--primary'] if sel else []),stdin=subprocess.PIPE,env=env)
  p.stdin.write(data);p.stdin.close();copies.append(p)
  until(2,sel)
 for sel in (1,0,1,0):
  send(3,sel,1);m=until(4,sel);assert m[2:]==(1,f'guest selection {sel}'.encode()),m
 print('PASS guest -> host CLIPBOARD and PRIMARY; independent retained offers')
 # Clear PRIMARY while normal clipboard remains usable.
 subprocess.run(['wl-copy','--primary','--clear'],env=env,check=True)
 until(5,1);send(3,1,1);assert until(4,1)[2:]==(0,b'')
 send(3,0,1);assert until(4,0)[3]==b'guest selection 0'
 print('PASS clearing PRIMARY releases only PRIMARY and prevents stale data')
 host_grab(0);host_grab(1)
 p0=paste(0);until(3,0);p1=paste(1);until(3,1)
 send(4,1,1,b'parallel primary');send(4,0,1,b'parallel clipboard')
 result(p1,b'parallel primary');result(p0,b'parallel clipboard')
 print('PASS simultaneous pastes with reversed reply order')
 p=paste(0);until(3,0)
 payload=b'large clipboard payload\n'*20000
 packet=struct.pack('<IIII',4,0,1,len(payload))+payload
 for offset in range(0,len(packet),8192):
  c.sendall(packet[offset:offset+8192]);time.sleep(.015)
 result(p,payload)
 print('PASS large payload with delayed chunks')
 send(5,1);time.sleep(.1)
 p=paste(0);until(3,0);send(4,0,1,b'clipboard survives primary release');result(p,b'clipboard survives primary release')
 print('PASS host PRIMARY release preserves normal clipboard')
 # Type mismatch closes the pending pipe without exposing mismatched data.
 p=paste(0);until(3,0);send(4,0,2,b'wrong type');out,err=p.communicate(timeout=5);assert out==b''
 print('PASS mismatched response type is rejected')
 # Pending request timeout retains the old bounded wait.
 p=paste(0);until(3,0);start=time.monotonic();out,err=p.communicate(timeout=5);assert out==b'' and time.monotonic()-start>=2
 print('PASS unanswered paste times out')
 send(2,9,0,struct.pack('<I',1));time.sleep(.1);assert agent.poll() is None
 print('PASS unsupported selection ignored')
 # Exercise the pre-existing binary MIME path as well as UTF-8 text.
 import base64
 png=base64.b64decode('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=')
 for sel in (0,1):
  send(2,sel,0,struct.pack('<I',2));time.sleep(.1)
  p=subprocess.Popen(['wl-paste','--no-newline','--type','image/png']+(['--primary'] if sel else []),stdout=subprocess.PIPE,stderr=subprocess.PIPE,env=env)
  assert until(3,sel)[2]==2;send(4,sel,2,png);result(p,png)
  p=subprocess.Popen(['wl-copy','--foreground','--type','image/png']+(['--primary'] if sel else []),stdin=subprocess.PIPE,env=env)
  p.stdin.write(png);p.stdin.close();copies.append(p);until(2,sel)
  send(3,sel,2);assert until(4,sel)[2:]==(2,png)
 print('PASS PNG binary transfers in both directions for both selections')
 host_grab(0);host_grab(1)
 p0=paste(0);until(3,0);p1=paste(1);until(3,1)
 send(12)
 for p in (p0,p1):
  out,err=p.communicate(timeout=5);assert out==b''
 print('PASS disconnect closes both pending pastes')
finally:
 agent.terminate();agent.wait(timeout=5)
 for p in locals().get('copies',[]):
  if p.poll() is None: p.terminate();p.wait(timeout=5)
 c.close();server.close();os.unlink(path);log.close()
