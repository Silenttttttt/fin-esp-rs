# Hold one fake-AC MQTT session open (deploy + real values) for N seconds, pinging, while heap is sampled.
import sys, time, json
sys.argv=[sys.argv[0]]+sys.argv[1:]
import fake_ac as F
secs=int(sys.argv[1]) if len(sys.argv)>1 else 90
real=[bytes.fromhex(f["rx"]) for f in json.load(open("frames.json")) if "rx" in f]
c=F.Conn(); c.send(F.connect(F.DID,30)); c.read_pkt(); c.send(F.subscribe(1,[f"lime/devices/{F.DID}"])); c.read_pkt()
c.send(F.publish(f"clip/provisioning/devices/{F.DID}", F.clip("deploy",{"appInfo":{"modelName":"RAC_0B0001_WW"}},0)))
for f in real: c.send(F.publish(f"clip/message/devices/{F.DID}", F.clip("device_packet", f.hex().upper())))
t0=time.time()
while time.time()-t0<secs:
    c.send(F.pkt(0xc0,b"")); c.read_pkt(5); time.sleep(10)
c.send(F.pkt(0xe0,b"")); c.close(); print("held",secs,"s")
