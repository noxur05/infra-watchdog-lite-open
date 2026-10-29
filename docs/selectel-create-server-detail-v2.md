# Selectel Cloud Server Creation - Detailed Documentation

Source: https://docs.selectel.ru/en/cloud-servers/create/create-server/
Captured: 2026-04-17

## Server Creation Overview

Selectel uses OpenStack under the hood. Cloud servers are created via:
- Control Panel (web UI)
- OpenStack CLI
- Terraform

## OpenStack CLI Command

```bash
openstack server create \
  [--image <image> | --volume <volume> | --snapshot <snapshot>] \
  --flavor <flavor> \
  --availability-zone <pool_segment> \
  --nic net-id=<net_uuid> \
  --security-group <security_group> \
  --key-name <key_name> \
  <server_name>
```

### Optional Parameters
- `--block-device-mapping vdb=<extra_volume>` - additional volumes
- `--property x_cloud_smt_enabled=false` - disable hyper-threading
- `--property x_cloud_numa_nodes=1` - single NUMA node
- `--tag <tag_name> --os-compute-api-version 2.52` - server tags
- `--tag preemptible --os-compute-api-version 2.72` - preemptible/interruptible server
- `--user-data <user_data.file>` - cloud-init script (Base64 encoded)

## Required Parameters

| Parameter | Description |
|-----------|-------------|
| `--image` / `--volume` / `--snapshot` | Boot source (image ID, volume ID, or snapshot ID) |
| `--flavor` | Server configuration (vCPUs, RAM) |
| `--availability-zone` | Pool segment (e.g., ru-1a, ru-2a) |
| `--nic net-id=<net_uuid>` | Network UUID to attach |
| `--security-group` | Security group ID or name |
| `--key-name` | SSH key pair name |

## Cloud-Init / User Data Support

**YES, cloud-init is supported.** From the docs:

> "To add a script that will be executed by the cloud-init agent when the operating system
> first starts up, in the Automation block in the User data field: open the Text tab and
> paste the script with text; or open the File tab and upload a file with the script."

CLI: `--user-data <user_data.file>` - path to script with Base64 encoded data.

**Root password:** The root password is auto-generated and shown in the control panel.
It is NOT set via cloud-init by default. However, since cloud-init is supported, you CAN
use a cloud-init script to set the root password:

```yaml
#cloud-config
password: your_password
chpasswd:
  expire: false
ssh_pwauth: true
```

## Networking

### Architecture (OpenStack Neutron)
Selectel networking is based on OpenStack Neutron. Objects:
- **Networks** - layer 2 domains
- **Subnets** - IP ranges within networks
- **Routers** - route traffic between subnets, provide internet access
- **Ports** - attachment points on servers
- **Security Groups** - traffic filtering rules

### Network Types
1. **Private Subnet** - no internet access, or with a static public IP
2. **Public Subnet** - all addresses directly internet-accessible

### Creating Network Infrastructure (Prerequisites for Server Creation)

**Step 1: Create a Network**
```bash
openstack network create <network_name>
```

**Step 2: Create a Subnet**
```bash
openstack subnet create \
  --subnet-range <cidr> \
  [--dhcp | --no-dhcp] \
  --gateway <gateway> \
  --network <network> \
  <subnet_name>
```

**Step 3: Create a Router (for internet access)**
```bash
openstack router create <router_name>
```

**Step 4: Connect Router to Subnet**
```bash
openstack router add subnet <router> <subnet>
```

**Step 5: Connect Router to External Network (internet)**
```bash
openstack router set --external-gateway external-network <router>
```

### Important Notes
- Subnets within same network need different CIDR prefixes
- DHCP creates two server ports, reserving first two IPs
- Router provides shared NAT for all devices behind it
- Router operates within single project and pool

## Public / Floating IPs

**Selectel uses OpenStack Floating IPs** (they call them "public IP addresses").

> "The address is floating (in API - Floating IP), as it can be quickly switched
> between devices in private subnets."

### Create a Floating IP
```bash
openstack floating ip create external-network
```
The IP is automatically allocated from the pool (cannot choose specific IP).

### Assign Floating IP to Server
```bash
# Find the server's port
openstack port list --server <server>

# Assign floating IP to port
openstack floating ip set --port <port_id> <public_ip_address>
```

### Other Floating IP Operations
```bash
# List all floating IPs
openstack floating ip list

# Disconnect from server
openstack floating ip unset --port <public_ip_address>

# Delete floating IP
openstack floating ip delete <public_ip_address>
```

### Limitations
- Floating IP can only be used within one project and one pool
- Bandwidth: 5 Gbps incoming in most regions

## Boot Sources

Available sources:
1. **Pre-built Image** - OS with pre-installed software
2. **Application** - marketplace apps
3. **Network Volume** - existing bootable volume
4. **Custom Image** - user-uploaded image
5. **Snapshot** - from existing server snapshot

GPU-optimized images are available with pre-installed drivers.

## Disk Configuration

- **Local SSD NVMe** - local disk (if available in configuration)
- **Network Disk** - remote block storage (types: Universal v2, SSD Fast v2, etc.)
- Can add additional network disks with custom IOPS settings

## Security Groups

Required for traffic filtering. Without security groups, no traffic is allowed.
If port security is disabled on the network, all traffic is allowed by default.

## Authentication Setup (OpenStack CLI)

### Environment Variables
```bash
export OS_AUTH_URL=<keystone_endpoint>  # Region-specific
export OS_IDENTITY_API_VERSION=3
export OS_VOLUME_API_VERSION=3
export OS_PROJECT_DOMAIN_NAME=<account_id>
export OS_PROJECT_ID=<project_id>
export OS_TENANT_ID=<project_id>
export OS_TENANT_NAME=<project_name>
export OS_REGION_NAME=<pool>  # e.g., ru-2
export OS_USER_DOMAIN_NAME=<account_id>
export OS_USERNAME=<service_user>
export OS_PASSWORD=<service_user_password>
```

### Getting Credentials
1. Create a service user in Control Panel > Account > Service Users
2. Assign "member" or "reader" role to the project
3. Download the RC file from the service user's Access tab
4. Source it: `source <rcfile_name>`

## Server Creation Flow Summary

1. Create service user + get OpenStack credentials
2. Create network + subnet
3. Create router + connect to subnet + set external gateway
4. Create security group with needed rules
5. Upload SSH key
6. Create server with: image, flavor, availability-zone, nic, security-group, key-name
7. (Optional) Create floating IP and assign to server port
8. (Optional) Pass cloud-init user-data for automation
