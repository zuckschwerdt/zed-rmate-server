# zed-rmate-server

A simple proof-of-concept rmate server for Zed.

Handles rmate TCP connections and uses Zed with tmp files.

This could be a lot more robust, feature full, safer, prettier. It could be generalized to support any editor.
But really is just intended to be a short-term stop-gap until proper support is added in Zed.

Note that error handling is a mess. It will basically panic on any problem. I might clean that up later…

## Usage

Note that on macOS you need to use `xattr -c zed-rmate-server` once after unzipping.

```
Usage: zed-rmate-server [OPTIONS]

Options:
  -z, --zed-bin <ZED_BIN>
          Sets the executable path for the Zed CLI binary

          [env: ZED_BIN=]
          [default: /usr/local/bin/zed]

  -b, --bind <BIND>
          Sets a custom rmate server address

          [env: RMATE_BIND=]
          [default: 127.0.0.1:52698]

  -o, --once
          End the server when Zed closes

          [env: RMATE_ONCE=]

  -h, --help
          Print help (see a summary with '-h')

  -V, --version
          Print version
```

## Program structure

Outline of the program structure and protocol handling as follows:

- Bind a TCP server, default is 127.0.0.1:52698 and listen for connections
- On each connection send a server identification
- Receive rmate commands of "open" or "." (end of commands)
- Receive command header of key-value pairs
- Copy body data to a temporary file
- Open the temporary file in Zed
- Watch the temporary file for changes, then send the file contents to the connection
- On Zed close or remote connection close remove the temporary files

## Protocol example

An example protocol flow is

```
$ sudo tcpflow -i lo0 -c port 52698

SERVER-CLIENT: Zed-Mate-Server 0

CLIENT-SERVER:
open
display-name: myremote:myfile.txt
real-path: /tmp/myfile.txt
data-on-save: yes
re-activate: yes
token: myfile.txt
(OPTIONAL) new: yes
(OPTIONAL) selection: 5
(OPTIONAL) file-type: text/plain
data: 26
Some random file contents

.

SERVER-CLIENT:
save
token: myfile.txt
data: 30
Di 16. Jul 00:11:08 CEST 2024
Modified random file contents

close
```

## License

Licensed under

 * MIT license ([LICENSE](LICENSE) or http://opensource.org/licenses/MIT)

#### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in the work by you, shall be licensed as above, without any additional terms or conditions.
