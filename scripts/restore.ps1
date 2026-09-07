<#
.SYNOPSIS
    Undo everything LinkSwitch can do to a machine, without needing LinkSwitch.

.DESCRIPTION
    LinkSwitch changes network interface metrics, registers scheduled tasks, and adds a
    startup entry. This script reverses all three. It is safe to run on a machine that
    never had LinkSwitch installed -- every step is conditional.

    By default only PHYSICAL adapters are restored. LinkSwitch never touches anything else,
    and a blanket reset would clear metrics that other software pinned deliberately: on the
    machine this was developed on, ProtonVPN and the Hyper-V Default Switch both carry
    manual metrics, and resetting the VPN's would break its routing.

    Prefer `linkswitch --uninstall` when the program is still available: that restores the
    exact values LinkSwitch recorded before it changed them, rather than falling back to
    Windows' automatic metric.

    Run with -WhatIf first to see what would change.

.PARAMETER All
    Also restore automatic metrics on virtual adapters -- VPN tunnels, Hyper-V switches,
    VMware adapters. Only use this if you know none of them needs its pinned metric.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\restore.ps1 -WhatIf

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File .\restore.ps1
#>
[CmdletBinding(SupportsShouldProcess)]
param(
    [switch]$All
)

$ErrorActionPreference = 'Continue'

function Test-Elevated {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    (New-Object Security.Principal.WindowsPrincipal($id)).IsInRole(
        [Security.Principal.WindowsBuiltInRole]::Administrator)
}

if (-not (Test-Elevated)) {
    Write-Warning 'This script needs an elevated PowerShell. Right-click PowerShell and choose "Run as administrator".'
    return
}

Write-Host 'LinkSwitch rescue' -ForegroundColor Cyan
Write-Host '-----------------'

# 1. Interface metrics -------------------------------------------------------------------
# LinkSwitch works by pinning a manual metric on the interface that should lose. Handing it
# back to Windows' automatic metric undoes that, whatever state it was left in.
#
# Scope matters here. LinkSwitch only ever pins metrics on physical Ethernet and Wi-Fi
# adapters, so restoring anything else can only do harm: VPN clients and virtual switches
# pin their own metrics on purpose, and clearing a tunnel's metric can break its routing.
$pinned = Get-NetIPInterface | Where-Object { $_.AutomaticMetric -eq 'Disabled' }
if (-not $All) {
    $physical = @(Get-NetAdapter -Physical -ErrorAction SilentlyContinue | Select-Object -ExpandProperty ifIndex)
    $skipped = @($pinned | Where-Object { $physical -notcontains $_.ifIndex })
    $pinned = @($pinned | Where-Object { $physical -contains $_.ifIndex })
    foreach ($s in ($skipped | Select-Object -ExpandProperty InterfaceAlias -Unique)) {
        Write-Host "Metrics:   skipping $s (not a physical adapter; LinkSwitch never changes these). Use -All to include it." -ForegroundColor DarkGray
    }
}
if (-not $pinned) {
    Write-Host 'Metrics:   nothing to restore; no physical adapter has a pinned metric.'
} else {
    foreach ($i in $pinned) {
        $label = "$($i.InterfaceAlias) ($($i.AddressFamily), currently $($i.InterfaceMetric))"
        if ($PSCmdlet.ShouldProcess($label, 'restore automatic metric')) {
            try {
                Set-NetIPInterface -InterfaceIndex $i.ifIndex -AddressFamily $i.AddressFamily `
                    -AutomaticMetric Enabled -ErrorAction Stop
                Write-Host "Metrics:   restored $label" -ForegroundColor Green
            } catch {
                Write-Warning "Metrics:   could not restore $label -- $($_.Exception.Message)"
            }
        }
    }
}

# 2. Scheduled tasks ---------------------------------------------------------------------
$tasks = Get-ScheduledTask -TaskPath '\LinkSwitch\' -ErrorAction SilentlyContinue
if (-not $tasks) {
    Write-Host 'Tasks:     none registered.'
} else {
    foreach ($t in $tasks) {
        if ($PSCmdlet.ShouldProcess($t.TaskName, 'unregister scheduled task')) {
            try {
                Unregister-ScheduledTask -TaskName $t.TaskName -TaskPath '\LinkSwitch\' `
                    -Confirm:$false -ErrorAction Stop
                Write-Host "Tasks:     removed $($t.TaskName)" -ForegroundColor Green
            } catch {
                Write-Warning "Tasks:     could not remove $($t.TaskName) -- $($_.Exception.Message)"
            }
        }
    }
}

# 3. Startup entry -----------------------------------------------------------------------
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
$existing = Get-ItemProperty -Path $runKey -Name 'LinkSwitch' -ErrorAction SilentlyContinue
if (-not $existing) {
    Write-Host 'Autostart: no entry.'
} elseif ($PSCmdlet.ShouldProcess('HKCU Run\LinkSwitch', 'remove startup entry')) {
    Remove-ItemProperty -Path $runKey -Name 'LinkSwitch' -ErrorAction SilentlyContinue
    Write-Host 'Autostart: removed.' -ForegroundColor Green
}

# 4. Connection-manager policy -----------------------------------------------------------
# Only reported, never changed. LinkSwitch touches this only when explicitly asked with
# --keep-wifi-connected, and an absent value means ENABLED rather than not-configured, so a
# blind "fix" here could quietly alter a machine's security posture. Deciding what this
# should be is the administrator's call, not a rescue script's.
$wcm = 'HKLM:\SOFTWARE\Policies\Microsoft\Windows\WcmSvc\GroupPolicy'
$v = (Get-ItemProperty -Path $wcm -Name 'fMinimizeConnections' -ErrorAction SilentlyContinue).fMinimizeConnections
if ($null -eq $v) {
    Write-Host 'Policy:    fMinimizeConnections is not set (Windows default: Wi-Fi will not auto-connect while Ethernet is up).'
} else {
    Write-Host "Policy:    fMinimizeConnections = $v. Not changed by this script."
    if ($v -eq 0) {
        Write-Host '           If LinkSwitch set this and you want the Windows default back, delete the value:' -ForegroundColor Yellow
        Write-Host "           Remove-ItemProperty -Path '$wcm' -Name fMinimizeConnections" -ForegroundColor Yellow
    }
}

# 5. Leftover files ----------------------------------------------------------------------
foreach ($p in @("$env:ProgramData\LinkSwitch", "$env:LOCALAPPDATA\LinkSwitch")) {
    if (Test-Path $p) {
        Write-Host "Files:     $p still exists (contains the log). Delete it manually when finished."
    }
}
if (Test-Path "$env:ProgramFiles\LinkSwitch") {
    Write-Host "Files:     $env:ProgramFiles\LinkSwitch still exists. Delete it to finish removing the program."
}

Write-Host ''
Write-Host 'Done. Current default routes:' -ForegroundColor Cyan
Get-NetRoute -DestinationPrefix '0.0.0.0/0' -ErrorAction SilentlyContinue |
    Sort-Object { $_.RouteMetric + $_.InterfaceMetric } |
    Format-Table ifIndex, InterfaceAlias, NextHop, RouteMetric, InterfaceMetric -AutoSize
