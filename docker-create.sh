#!/bin/bash
docker run -d \
    --name stouter \
    --network host \
    -v /srv/stouter/stouter.json:/srv/stouter/stouter.json \
    stouter --config /srv/stouter/stouter.json

