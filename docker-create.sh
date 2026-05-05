#!/bin/bash
docker run -d \
    --name stouter \
    --network host \
    -v /srv/stouter/config/:/srv/stouter/config/ \
    stouter --config /srv/stouter/config/stouter.json

