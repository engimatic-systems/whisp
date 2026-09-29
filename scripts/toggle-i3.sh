#!/usr/bin/env bash
set -euo pipefail

exec 9>"${XDG_RUNTIME_DIR:?}/i3-whisp.lock"
flock -n 9 || exit 0

tree=$(i3-msg -r -t get_tree)
commands=$(jq -r '
    [
        .. | objects | select(.type? == "workspace")
        | any(.. | objects; .focused? == true) as $current
        | .. | objects
        | select(.window? != null)
        | select(.window_properties.class? == "whisp" and .name == "Whisp")
        | {id, current: $current}
    ] as $windows
    | any($windows[]; .current) as $hide
    | $windows
    | map("[con_id=\(.id)] move scratchpad"
          + if $hide then ""
            else ", scratchpad show, resize set 512 px 384 px" end)
    | join("; ")
' <<< "$tree")

if [[ -n "$commands" ]]; then
    i3-msg "$commands" >/dev/null
else
    # GTK's application ID makes repeated launches reuse the same process.
    bash "$(dirname -- "${BASH_SOURCE[0]}")/whisp" 9>&- &
fi
