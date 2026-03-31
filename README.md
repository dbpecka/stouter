# stouter
Light-weight service discovery and tunneling

## Overview
Stouter securely exposes your services over untrusted networks. It allows you to keep your services
exposed internally and to securely tunnel them to trusted hosts.

## Architecture
Built entirely in Rust, Stouter is built to be highly reliable and performant. Gossip protocol is 
used between members nodes to listen for configuration changes.

There are two main components: the *node daemon* and the *subscribe daemon*. The node daemon is run on
hosts which are running local services to be exposed. The subscribe daemon is run on hosts which will directly
access those services.

Both daemons are responsible for listening for incoming gossip messages and updating the dynamic configuration.

Management commands, such as adding a new service, are run using a cli tool, which publishes gossip messages to
current member nodes.

### Configuration File
`````
{
    "clusterSecret": "mySecret",
    "mode": "nodeOrSubscribe",
    "bind": "0.0.0.0:8080",
    "localSecret": "mySecret",
    "knownNodes": ["appHostA:8080", "appHostB"],
    "dynamicConfig": {
        "serviceGroups": [
            {
                "name": "homeServices",
                "services": [
                    {
                        "name": "homeAssistant",
                        "nodeId": "appHostA",
                        "nodePort": 8123,
                        
                    }
                ]
            }
        ]
    }
}
`````

## Benefits



## Usage

### Running the node daemon
`stouter node`
Starts the daemon and listens for incoming gossip messages.

### Running the subscribe daemon
`stouter subscribe`
Starts the daemon and listens for incoming gossip messages to get the latest config. For each service, binds to a local
port and tunnels the service. The subscribe daemon also runs a lightweight DNS server that can be used to resolve
services based on their name.

### Adding a node
`stouter node add`
Adds the current host to the cluster. Generates a new localSecret.

### Adding a service
`stouter service add --group groupName --name serviceName --port intPort`
Add a service which is running on the current node. Sends a gossip message to all nodes, 
which persist the new config.

### List services 
`stouter service list`
Lists the services that are in the currently-running dynamicConfig.

## Integration

[//]: # (- Traefik)

[//]: # (- DNS)


