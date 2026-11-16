# A single bounded non-PTY exec. Only shell builtins and one sleep are needed.
# No files are written and no process or filesystem enumeration is performed.
LC_ALL=C
export LC_ALL
[ -r /proc/stat ] && [ -r /proc/meminfo ] && [ -r /proc/loadavg ] && [ -r /proc/uptime ] || exit 74
printf 'CSHELL-RESOURCES-1\n'
IFS= read -r line < /proc/stat || exit 74
printf '%s\n' "$line"
sleep 1 || exit 74
IFS= read -r line < /proc/stat || exit 74
printf '%s\n' "$line"
while IFS= read -r line; do
    case "$line" in
        MemTotal:*|MemAvailable:*|SwapTotal:*|SwapFree:*) printf '%s\n' "$line" ;;
    esac
done < /proc/meminfo
IFS= read -r line < /proc/loadavg || exit 74
printf 'load %s\n' "$line"
IFS= read -r line < /proc/uptime || exit 74
printf 'uptime %s\n' "$line"
printf 'END-CSHELL-RESOURCES\n'
