# ToyConsul

A toy eventually consistent distributed registry.

## Installation

```bash
cargo install --path .
```

## Run

- In the first terminal

  ```bash
  toyconsul start --id instance1 --addr 127.0.0.1:7001 --rpc-addr toyconsul.instance1.sock
  ```

- In the second terminal

  - Start instance 2

    ```bash
    toyconsul start --id instance2 --addr 127.0.0.1:7002 --rpc-addr toyconsul.instance2.sock
    ```
  
  - Send the join command to instance2 and let it join to instance1
  
    ```bash
    toyconsul join --id instance1 --addr 127.0.0.1:7001 --rpc-addr toyconsul.instance2.sock
    ```

- In the third terminal

  - Start instance 3

    ```bash
    toyconsul start --id instance3 --addr 127.0.0.1:7003 --rpc-addr toyconsul.instance3.sock
    ```
  
  - Send the join command to instance3 and let it join to instance1 (can also join to instance 2)
  
    ```bash
    toyconsul join --id instance1 --addr 127.0.0.1:7001 --rpc-addr toyconsul.instance3.sock
    ```

- In the fourth terminal

  - Insert a key - value to the instance1

    ```bash
    toyconsul register --name web --addr 192.0.0.1:8080 --rpc-addr toyconsul.instance1.sock
    toyconsul register --name db --addr 192.0.0.1:8081 --rpc-addr toyconsul.instance2.sock
    ```
  
  - After some seconds, you can get the value from any one of three instances

    ```bash
    toyconsul list --rpc-addr toyconsul.instance1.sock
    ```

    ```bash
    toyconsul list --rpc-addr toyconsul.instance2.sock
    ```

    ```bash
    toyconsul list --rpc-addr toyconsul.instance3.sock
    ```
