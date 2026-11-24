# dispatch-nest

A [nuthatch](https://nuthatch-indexer.com) nest of `RPCDataService`'s registration and service events on Arbitrum One. The gateway reads providers from it with `[discovery] source = "nest"`.

`abis/dispatch.json` is taken from the compiled contract: `ProviderRegistered`, `ProviderDeregistered`, `ServiceStarted`, `ServiceStopped` and `PaymentsDestinationSet`. Horizon's base interface also declares `ServiceStarted(address,bytes)` and `ServiceStopped(address,bytes)`; the contract never emits them, so they are left out.

Run it beside the gateway, on the same Docker network and published nowhere:

```yaml
  nest:
    image: ubuntu:24.04
    restart: unless-stopped
    env_file: [ "./secrets/nest-rpc.env" ]      # NEST_RPC=<your Arbitrum RPC URL>
    volumes:
      - "/usr/local/bin/nuthatch:/usr/local/bin/nuthatch:ro"
      - "/etc/ssl/certs:/etc/ssl/certs:ro"
      - "./nest:/nest"
    command: [ "sh", "-c", "exec nuthatch dev --rpc \"$$NEST_RPC\" --dir /nest --listen 0.0.0.0:8120 --window 9000000 --poll-interval 1m" ]
```

and set `nest_url = "http://nest:8120"` in the gateway's `[discovery]`. The first run backfills from block 456,917,519, about 70 million blocks of a contract with a few dozen events; it took about half an hour on a keyed RPC. Avoid `arb1.arbitrum.io` in the pool: it sends no log timestamps, and nuthatch then fetches every header in batches of ten.
