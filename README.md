# HTTPing

HTTPing probes an HTTP or HTTPS endpoint and reports response latency.

## Installation

Download an archive or run an installer from the
[latest GitHub Release](https://github.com/stianfro/httping/releases/latest).

On macOS or Linux:

```sh
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/stianfro/httping/releases/download/v0.1.0/httping-installer.sh | sh
```

On Windows PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/stianfro/httping/releases/download/v0.1.0/httping-installer.ps1 | iex"
```

## Usage

```text
Usage: httping [OPTIONS] <URL>

Arguments:
  <URL>  HTTP or HTTPS URL to probe

Options:
  -c, --count <COUNT>        Number of probes to send. By default, probes continue until Ctrl-C
  -i, --interval <INTERVAL>  Minimum time between probe starts [default: 1s]
  -t, --timeout <TIMEOUT>    Maximum time for DNS setup and for each probe [default: 10s]
  -h, --help                 Print help
  -V, --version              Print version
```

Durations accept values such as `250ms`, `1s`, and `2m`.

```console
$ httping --count 2 https://example.com
HTTPing https://example.com/
resolved: 93.184.216.34:443
2026-07-28T12:00:00.000Z 200 OK 124ms
2026-07-28T12:00:01.000Z 200 OK 42ms

--- httping statistics ---
attempts: 2, responses: 2, loss: 0.0%
time: min 42ms, avg 83ms, max 124ms
```

HTTPing starts the first probe immediately. It continues after request errors.
A finite run exits with status 1 if any probe fails. Pressing Ctrl-C prints the
summary and exits with status 0.
