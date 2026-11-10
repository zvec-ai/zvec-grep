$ErrorActionPreference = "Stop"

# PowerShell decodes redirected stdin before forwarding it to npm's native
# command. MCP uses UTF-8, even when the Windows console defaults to CP437.
$utf8 = [System.Text.UTF8Encoding]::new($false)
[Console]::InputEncoding = $utf8
[Console]::OutputEncoding = $utf8
$OutputEncoding = $utf8

# Keep arguments as data and exercise the installed npm shim itself.
$entry = $args[0]
$entryArguments = @($args | Select-Object -Skip 1)
& $entry @entryArguments
exit $LASTEXITCODE
