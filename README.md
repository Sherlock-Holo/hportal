# hportal

Holo portal

a simple reverse proxy, very simple, like a portal: you connect the hportal server bind addr, then all your tcp flow
will transfer to your real server

## why I write a new reverse proxy?

actually I have used the frp a long time, until my Archlinux pacman updates it to 0.53, then my frpc is broken, I read
its new example config, that's so complicated, I don't want to spend more then 5min to understand how to make it work
again

## how simple is it?

### config file is simple

a simple reverse proxy should a have simple config file

the server config example:

```yaml
mode: server
server:
  server_addr: 0.0.0.0:9666
  tls: # tls config is optional
    key: path-to-key-file
    cert: path-to-cert-file
  peers:
    - name: example-a
      secret: AiPrLrRCLa3Q-b7LHn5zPGGfi6PjZZFhuul6PWyj9S0

    - name: example-b
      secret: c-bzcbluG8SKcLI7MddKI15FKqQIcnlJtU9PdjZOlpA
```

the client config example

```yaml
mode: client
client:
  name: example-a
  secret: AiPrLrRCLa3Q-b7LHn5zPGGfi6PjZZFhuul6PWyj9S0
  server_addr: http://localhost:9666
  forwards:
    - name: web
      local_addr: 127.0.0.1:8000
      remote_addr: 0.0.0.0:8001
```

### proto is simple

I use grpc+protobuf3 directly, it has bi-flow support, so I don't need to do a lot of dirty jobs to let server tell
client
"there is a new connect request, please help me connect it"

and now many http reverse proxy server support grpc, you can combine hportal with your http reverse proxy server

### start client/server is simple

```shell
hportal is a simple reverse proxy

Usage: hportal [OPTIONS] --config <CONFIG>

Options:
  -c, --config <CONFIG>  config path
  -d, --debug            enable debug log
  -h, --help             Print help
```

you just need `hportal -c path-to-config` to start hportal

### build is simple

just run `cargo build -r`
