#!/bin/bash
# MQTT over TLS: runs a private mosquitto on 8883 with a self-signed CA,
# points both test instances at it (enable_tls) and sends a JSON-RPC
# request over MQTT to each, then restores the configs.
cd "$(dirname "$0")"
T=/tmp/mrtls
rm -rf $T && mkdir -p $T
openssl req -x509 -newkey rsa:2048 -nodes -keyout $T/ca.key -out $T/ca.pem -days 2 -subj "/CN=mrtest-ca" 2>/dev/null
openssl req -newkey rsa:2048 -nodes -keyout $T/srv.key -out $T/srv.csr -subj "/CN=localhost" 2>/dev/null
printf "subjectAltName=DNS:localhost\n" > $T/ext.cnf
openssl x509 -req -in $T/srv.csr -CA $T/ca.pem -CAkey $T/ca.key -CAcreateserial -out $T/srv.pem -days 2 -extfile $T/ext.cnf 2>/dev/null
# mosquitto drops privileges, the key must be readable by its user
chmod 755 $T && chmod 644 $T/*
cat > $T/mosq.conf <<EOF
listener 8883 127.0.0.1
cafile $T/ca.pem
certfile $T/srv.pem
keyfile $T/srv.key
allow_anonymous true
EOF
mosquitto -c $T/mosq.conf > $T/mosq.log 2>&1 &
MOSQ=$!
sleep 1
for i in ref rust; do
    D=~/mr/inst/$i
    cp $D/config/moonraker.conf $D/config/moonraker.conf.bak
    python3 - "$D/config/moonraker.conf" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
s = s.replace("[mqtt]\naddress: localhost\n", "[mqtt]\naddress: localhost\nport: 8883\nenable_tls: True\n")
open(p, "w").write(s)
PY
done
export SSL_CERT_FILE=$T/ca.pem
bash reset_both.sh > /dev/null
sleep 2
for i in ref rust; do
    echo "=== $i"
    grep -E "MQTT|mqtt" ~/mr/inst/$i/logs/moonraker.log | tail -3 | cut -c1-160
    mosquitto_sub --cafile $T/ca.pem -h localhost -p 8883 -t "$i/moonraker/api/response" -C 1 -W 5 > $T/resp_$i.txt &
    sleep 0.5
    mosquitto_pub --cafile $T/ca.pem -h localhost -p 8883 -t "$i/moonraker/api/request" -m '{"jsonrpc":"2.0","method":"printer.objects.query","params":{"objects":{"webhooks":null}},"id":7}'
    wait
    sed -e 's/"eventtime":[0-9.]*//' $T/resp_$i.txt
done
for i in ref rust; do
    D=~/mr/inst/$i
    mv $D/config/moonraker.conf.bak $D/config/moonraker.conf
done
kill $MOSQ
unset SSL_CERT_FILE
bash reset_both.sh > /dev/null
