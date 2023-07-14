#!/bin/sh

RPC_USER="${RPC_USER:=serai}"
RPC_PASS="${RPC_PASS:=seraidex}"

# Run Monero
# TODO: Restore Auth
# https://www.getmonero.org/resources/developer-guides/daemon-rpc.html
# --regest: It runs in a "regression testing mode", which allows you to create a fake blockchain (i.e. it is not validated by other nodes), useful only for development and testing.
# --offline: ensures that the node does not connect to the main network and learn of its latest chaintip
# --fixed-difficulty: keeps the difficulty constant, allowing a large number of blocks to be generated quickly.
# --confirm-external-bind: Here, if you really intend the RPC to be accessible from other machines
# --rpc-access-control-origins: Specify a comma separated list of origins to allow cross origin resource sharing
monerod --regtest --offline --fixed-difficulty=1 \
  --rpc-bind-ip=0.0.0.0 --rpc-access-control-origins * --confirm-external-bind \
  --non-interactive
