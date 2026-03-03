#Requires -Version 7
<#
.SYNOPSIS
    Shared environment defaults for lm-gateway tools.
.DESCRIPTION
    Copy this file to tools/.env.ps1 (gitignored) and fill in your values.
    Scripts dot-source this file to pick up defaults without requiring flags every run.
.NOTES
    Also configure ~/.ssh/config with a Host entry named $DefaultSshAlias:

    Host cortex
      HostName <your-proxmox-ip-or-hostname>
      User root
      IdentityFile ~/.ssh/<your-key>
#>

# Proxmox SSH alias — must match a Host entry in ~/.ssh/config
$DefaultSshAlias = 'cortex'

# LXC container ID for the language / lm-gateway container
$DefaultLxcId = 200

# Absolute path to the lm-gateway source inside the LXC container
$DefaultLmGatewayPath = '/opt/lm-gateway'
