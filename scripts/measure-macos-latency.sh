#!/bin/sh
set -eu

duration=${1:-15}
label=${PADJUTSU_LAUNCHD_LABEL:-me.gulya.padjutsu}
domain="gui/$(id -u)"
service="$domain/$label"
error_log=${PADJUTSU_ERROR_LOG:-/tmp/padjutsu.err}

case "$duration" in
    ''|*[!0-9]*)
        echo "duration must be a positive integer (seconds)" >&2
        exit 2
        ;;
esac

if [ "$duration" -lt 5 ]; then
    echo "duration must be at least 5 seconds so metrics can report" >&2
    exit 2
fi

cleanup() {
    launchctl unsetenv PADJUTSU_METRICS 2>/dev/null || true
    launchctl kickstart -k "$service" >/dev/null 2>&1 || true
}
trap cleanup EXIT INT TERM

start_line=0
if [ -f "$error_log" ]; then
    start_line=$(wc -l < "$error_log" | tr -d ' ')
fi

launchctl setenv PADJUTSU_METRICS 1
launchctl kickstart -k "$service"

pid=''
attempt=0
while [ "$attempt" -lt 100 ]; do
    candidate=$(launchctl print "$service" 2>/dev/null | awk '/^[[:space:]]*pid = / { print $3; exit }')
    command=''
    if [ -n "$candidate" ]; then
        command=$(ps -p "$candidate" -o command= 2>/dev/null || true)
    fi
    case "$command" in
        *padjutsud*)
            pid=$candidate
            ;;
    esac
    if [ -n "$pid" ]; then
        break
    fi
    attempt=$((attempt + 1))
    sleep 0.1
done

if [ -z "$pid" ]; then
    echo "could not find $label pid after restart" >&2
    exit 1
fi

sample_file="/tmp/padjutsud-latency-$pid.sample"
echo "Measuring padjutsud pid=$pid for ${duration}s. Move the mouse stick now."
sample "$pid" "$duration" -file "$sample_file" >/dev/null

echo
echo "Metrics:"
if [ -f "$error_log" ]; then
    sed -n "$((start_line + 1)),\$p" "$error_log" \
        | grep -E '\[(padjutsu|performer|stick)-metrics\]' \
        || echo "No active stick samples were recorded."
else
    echo "No error log found at $error_log."
fi

display_queries=$(grep -c 'SLSGetDisplayBounds' "$sample_file" || true)
event_posts=$(grep -c 'SLEventPost' "$sample_file" || true)
echo
echo "Sample: $sample_file"
echo "Hot-stack occurrences: display_bounds=$display_queries event_post=$event_posts"
