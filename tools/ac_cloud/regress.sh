#!/bin/bash
# Fin-ESP regression check (non-destructive): identity, dashboard, status, lamp read path, AC/IR state, ac_cloud status.
IP=192.168.1.240
echo "== identity"; ip neigh show $IP | grep -o "00:70:07:25:6d:d4" || echo "MAC MISMATCH"
echo "== dashboard"; curl -s -m 8 http://$IP/ | grep -o '<title>[^<]*' ; curl -s -m 8 -o /dev/null -w "bytes=%{size_download} http=%{http_code} t=%{time_total}s\n" http://$IP/
echo "== status"; curl -s -m 8 http://$IP/status | python3 -c 'import json,sys;d=json.load(sys.stdin);print({k:d[k] for k in ["wifi","lamp_on","lamp_known","display_on"]}, "ac=",{k:d["ac"][k] for k in ["on","mode","temp","sent","pending"]}, "mem=",d["mem"])'
echo "== ir"; curl -s -m 5 http://$IP/ir/status | tr '\n' ' '; echo
echo "== ac/state"; curl -s -m 5 http://$IP/ac/state; echo
echo "== ac/cloud"; curl -s -m 5 http://$IP/ac/cloud | head -c 700; echo
