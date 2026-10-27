#!/bin/bash
# mosquitto client in an ephemeral container; rethink.internal -> Fin-ESP so hostname verification is real.
# Set RETHINK_CA to your CA cert path (the one Fin-ESP's TLS cert chains to) and FIN_ESP_IP if not .240.
CA="${RETHINK_CA:-./ca.cert}"
IP="${FIN_ESP_IP:-192.168.1.240}"
exec docker run --rm --network host --add-host "rethink.internal:$IP" -v "$CA:/ca.cert:ro" eclipse-mosquitto:2 "$@"
