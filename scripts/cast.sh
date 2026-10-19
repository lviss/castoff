#!/usr/bin/env bash
# Casts to the castoff daemon from a computer: a command-line reproduction of
# castoff-android's FCastClient.kt command surface (play/pause/resume/stop/
# seek/volume/ping, the queue extension, and the image-upload+wallpaper
# extension), for controlling a box or driving manual tests without the
# Android app. See README's "Casting from a computer" section.
set -euo pipefail

die() {
    echo "cast.sh: $*" >&2
    exit 1
}

# Checked before anything else runs: without this, a missing tool fails
# deep inside some helper function with a raw, unexplained shell
# "command not found" (e.g. "./scripts/cast.sh: line N: jq: command not
# found") that gives no hint what's missing or how to fix it. `socat` is
# deliberately not checked here -- it's not actually a cast.sh dependency
# (fcast-client.pl talks TCP directly via Perl's IO::Socket::INET; socat only
# appears in README's own raw-frame manual-protocol-test examples).
check_dependencies() {
    local tool missing=()
    for tool in perl jq curl; do
        command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
    done
    if [[ ${#missing[@]} -gt 0 ]]; then
        die "missing required tool(s): ${missing[*]}." \
            "Run this from 'nix develop' in the repo root (its devShells.default provides" \
            "perl/jq/curl), or install ${missing[*]} directly."
    fi
}
check_dependencies

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FCAST_CLIENT="$SCRIPT_DIR/lib/fcast-client.pl"

HOST="${CASTOFF_HOST:-127.0.0.1}"
PORT="${CASTOFF_PORT:-46899}"
IMAGE_PORT="${CASTOFF_IMAGE_PORT:-46900}"

# FCast opcodes this talks, matching daemon/src/fcast.rs's Opcode enum.
OPCODE_PLAY=1
OPCODE_PAUSE=2
OPCODE_RESUME=3
OPCODE_STOP=4
OPCODE_SEEK=5
OPCODE_SET_VOLUME=8
OPCODE_PING=12
OPCODE_PONG=13
OPCODE_REQUEST_QUEUE=14
OPCODE_QUEUE_JUMP_FORWARD=16
OPCODE_QUEUE_JUMP_BACKWARD=17
OPCODE_CLEAR_QUEUE=18
OPCODE_QUEUE_JUMP_TO_INDEX=19
OPCODE_SET_IMAGE_WALLPAPER=20
OPCODE_REMOTE_COMMAND=22

usage() {
    cat <<'EOF'
Usage: cast.sh [--host HOST] [--port PORT] [--image-port PORT] <command> [args...]

Controls the castoff daemon over FCast, the same command surface the Android
app has. Global flags (or CASTOFF_HOST/CASTOFF_PORT/CASTOFF_IMAGE_PORT) must
come before the command; defaults are 127.0.0.1:46899 (FCast) / :46900 (image
uploads).

Playback:
  play URL [--container TYPE] [--time SECS] [--volume 0.0-1.0] [--speed MULT]
            [--header KEY=VALUE ...]
  pause
  resume
  stop
  seek SECONDS
  volume 0.0-1.0
  ping

Queue (castoff private extension):
  queue-request          show the current queue
  queue-next             jump to the next queue item
  queue-prev             jump to the previous queue item
  queue-clear            empty the queue (stops playback first if current)
  queue-jump INDEX       jump straight to an index in the queue

Remote navigation (castoff private extension; dispatched to whichever source
currently accepts one, e.g. a displayed Jellyfin page):
  remote up|down|left|right|select|back

Images (castoff private extension; upload travels over HTTP, not FCast):
  upload-image FILE [--content-type TYPE] [--display] [--wallpaper]
            uploads FILE, printing the daemon's {id,url,container}; with
            --display, also plays it immediately; with --wallpaper, also
            tags it for idle-screen wallpaper rotation
  set-wallpaper ID [--untag]
            tags (or, with --untag, untags) a previously uploaded image id
            for idle-screen wallpaper rotation

  -h, --help             show this help
EOF
}

# Sends one FCast command and prints its reply as "OPCODE<TAB>BODY".
send_command() {
    local opcode="$1" body="${2:-}"
    perl "$FCAST_CLIENT" --host "$HOST" --port "$PORT" --opcode "$opcode" --body "$body"
}

# Same, but keeps reading past an interleaved PlaybackUpdate until the real
# QueueState reply arrives -- see fcast-client.pl's read_queue_reply.
send_queue_command() {
    local opcode="$1" body="${2:-}"
    perl "$FCAST_CLIENT" --host "$HOST" --port "$PORT" --opcode "$opcode" --body "$body" --queue-reply
}

# Prints a command reply's body as pretty JSON, or a short note if it had
# none (e.g. Pong).
print_reply() {
    local reply="$1"
    local body="${reply#*$'\t'}"
    if [[ -n "$body" ]]; then
        echo "$body" | jq .
    else
        echo "ok"
    fi
}

cmd_play() {
    local url="" container="" time="" volume="" speed=""
    local -a headers=()
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --container) container="${2:?--container requires a value}"; shift 2 ;;
            --time) time="${2:?--time requires a value}"; shift 2 ;;
            --volume) volume="${2:?--volume requires a value}"; shift 2 ;;
            --speed) speed="${2:?--speed requires a value}"; shift 2 ;;
            --header) headers+=("${2:?--header requires KEY=VALUE}"); shift 2 ;;
            -h|--help) usage; exit 0 ;;
            -*) die "play: unknown flag $1" ;;
            *)
                [[ -z "$url" ]] || die "play: unexpected extra argument $1"
                url="$1"
                shift
                ;;
        esac
    done
    [[ -n "$url" ]] || die "play requires a URL"

    local headers_json="null"
    if [[ ${#headers[@]} -gt 0 ]]; then
        for header in "${headers[@]}"; do
            [[ "$header" =~ ^[^=]+=.*$ ]] || die "play: --header values must be KEY=VALUE (got: $header)"
        done
        headers_json="$(printf '%s\n' "${headers[@]}" \
            | jq -R 'capture("^(?<k>[^=]+)=(?<v>.*)$") | {(.k): .v}' \
            | jq -s 'add')" \
            || die "play: --header values must be KEY=VALUE"
    fi

    local body
    body="$(jq -nc \
        --arg url "$url" \
        --arg container "$container" \
        --arg time "$time" \
        --arg volume "$volume" \
        --arg speed "$speed" \
        --argjson headers "$headers_json" \
        '{url: $url}
         + (if $container != "" then {container: $container} else {} end)
         + (if $time != "" then {time: ($time | tonumber)} else {} end)
         + (if $volume != "" then {volume: ($volume | tonumber)} else {} end)
         + (if $speed != "" then {speed: ($speed | tonumber)} else {} end)
         + (if $headers != null then {headers: $headers} else {} end)')" \
        || die "play: could not build the Play body"

    local reply
    reply="$(send_command "$OPCODE_PLAY" "$body")" || die "play command failed"
    print_reply "$reply"
}

cmd_pause() {
    local reply
    reply="$(send_command "$OPCODE_PAUSE")" || die "pause command failed"
    print_reply "$reply"
}

cmd_resume() {
    local reply
    reply="$(send_command "$OPCODE_RESUME")" || die "resume command failed"
    print_reply "$reply"
}

cmd_stop() {
    local reply
    reply="$(send_command "$OPCODE_STOP")" || die "stop command failed"
    print_reply "$reply"
}

cmd_seek() {
    local secs="${1:-}"
    [[ -n "$secs" ]] || die "seek requires a time in seconds"
    local body
    body="$(jq -nc --argjson time "$secs" '{time: $time}')" || die "seek: invalid time '$secs'"
    local reply
    reply="$(send_command "$OPCODE_SEEK" "$body")" || die "seek command failed"
    print_reply "$reply"
}

cmd_volume() {
    local level="${1:-}"
    [[ -n "$level" ]] || die "volume requires a level between 0.0 and 1.0"
    local body
    body="$(jq -nc --argjson volume "$level" '{volume: $volume}')" || die "volume: invalid level '$level'"
    local reply
    reply="$(send_command "$OPCODE_SET_VOLUME" "$body")" || die "volume command failed"
    print_reply "$reply"
}

cmd_ping() {
    local reply opcode
    reply="$(send_command "$OPCODE_PING")" || die "ping command failed"
    opcode="${reply%%$'\t'*}"
    [[ "$opcode" == "$OPCODE_PONG" ]] || die "ping: unexpected reply opcode $opcode"
    echo "pong"
}

cmd_queue_request() {
    local reply
    reply="$(send_queue_command "$OPCODE_REQUEST_QUEUE")" || die "queue-request command failed"
    print_reply "$reply"
}

cmd_queue_next() {
    local reply
    reply="$(send_queue_command "$OPCODE_QUEUE_JUMP_FORWARD")" || die "queue-next command failed"
    print_reply "$reply"
}

cmd_queue_prev() {
    local reply
    reply="$(send_queue_command "$OPCODE_QUEUE_JUMP_BACKWARD")" || die "queue-prev command failed"
    print_reply "$reply"
}

cmd_queue_clear() {
    local reply
    reply="$(send_queue_command "$OPCODE_CLEAR_QUEUE")" || die "queue-clear command failed"
    print_reply "$reply"
}

cmd_queue_jump() {
    local index="${1:-}"
    [[ -n "$index" ]] || die "queue-jump requires an index"
    local body
    body="$(jq -nc --argjson index "$index" '{index: $index}')" || die "queue-jump: invalid index '$index'"
    local reply
    reply="$(send_queue_command "$OPCODE_QUEUE_JUMP_TO_INDEX" "$body")" || die "queue-jump command failed"
    print_reply "$reply"
}

# Maps a human-friendly direction to RemoteCommandMessage's command value
# (daemon/src/fcast.rs's RemoteCommand enum), used by cmd_remote.
remote_command_name() {
    case "$1" in
        up) echo "MoveUp" ;;
        down) echo "MoveDown" ;;
        left) echo "MoveLeft" ;;
        right) echo "MoveRight" ;;
        select) echo "Select" ;;
        back) echo "Back" ;;
        *) die "remote: unknown direction '$1' (want up|down|left|right|select|back)" ;;
    esac
}

cmd_remote() {
    local direction="${1:-}"
    [[ -n "$direction" ]] || die "remote requires a direction (up|down|left|right|select|back)"
    local command
    command="$(remote_command_name "$direction")"
    local body
    body="$(jq -nc --arg command "$command" '{command: $command}')" || die "remote: could not build the RemoteCommand body"
    local reply
    reply="$(send_command "$OPCODE_REMOTE_COMMAND" "$body")" || die "remote command failed"
    print_reply "$reply"
}

# Best-effort Content-Type guess from a file's extension, for upload-image
# when --content-type isn't given. The daemon only checks the type starts
# with "image/" (daemon/src/upload.rs), so an exact subtype match isn't
# essential, but a sensible one keeps the uploaded file openable elsewhere.
guess_content_type() {
    local file="$1"
    case "${file,,}" in
        *.jpg|*.jpeg) echo "image/jpeg" ;;
        *.png) echo "image/png" ;;
        *.gif) echo "image/gif" ;;
        *.webp) echo "image/webp" ;;
        *.bmp) echo "image/bmp" ;;
        *) echo "image/jpeg" ;;
    esac
}

cmd_upload_image() {
    local file="" content_type="" display=0 wallpaper=0
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --content-type) content_type="${2:?--content-type requires a value}"; shift 2 ;;
            --display) display=1; shift ;;
            --wallpaper) wallpaper=1; shift ;;
            -h|--help) usage; exit 0 ;;
            -*) die "upload-image: unknown flag $1" ;;
            *)
                [[ -z "$file" ]] || die "upload-image: unexpected extra argument $1"
                file="$1"
                shift
                ;;
        esac
    done
    [[ -n "$file" ]] || die "upload-image requires a file path"
    [[ -f "$file" ]] || die "upload-image: no such file: $file"
    [[ -n "$content_type" ]] || content_type="$(guess_content_type "$file")"

    local resp
    resp="$(curl -sS -X POST --data-binary "@$file" -H "Content-Type: $content_type" \
        "http://$HOST:$IMAGE_PORT/images")" \
        || die "upload to http://$HOST:$IMAGE_PORT/images failed"
    echo "$resp" | jq . || die "upload-image: daemon did not return JSON: $resp"

    local id url container
    id="$(jq -r '.id // empty' <<<"$resp")"
    url="$(jq -r '.url // empty' <<<"$resp")"
    container="$(jq -r '.container // empty' <<<"$resp")"
    [[ -n "$id" ]] || die "upload-image: daemon's response had no id: $resp"

    if [[ "$display" -eq 1 ]]; then
        local play_body reply
        play_body="$(jq -nc --arg url "$url" --arg container "$container" '{url: $url, container: $container}')"
        reply="$(send_command "$OPCODE_PLAY" "$play_body")" || die "upload-image: display Play failed"
        print_reply "$reply"
    fi

    if [[ "$wallpaper" -eq 1 ]]; then
        local wallpaper_body reply
        wallpaper_body="$(jq -nc --arg id "$id" '{id: $id, wallpaper: true}')"
        reply="$(send_command "$OPCODE_SET_IMAGE_WALLPAPER" "$wallpaper_body")" \
            || die "upload-image: SetImageWallpaper failed"
        print_reply "$reply"
    fi
}

cmd_set_wallpaper() {
    local id="" wallpaper="true"
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --untag) wallpaper="false"; shift ;;
            -h|--help) usage; exit 0 ;;
            -*) die "set-wallpaper: unknown flag $1" ;;
            *)
                [[ -z "$id" ]] || die "set-wallpaper: unexpected extra argument $1"
                id="$1"
                shift
                ;;
        esac
    done
    [[ -n "$id" ]] || die "set-wallpaper requires an image id"

    local body reply
    body="$(jq -nc --arg id "$id" --argjson wallpaper "$wallpaper" '{id: $id, wallpaper: $wallpaper}')"
    reply="$(send_command "$OPCODE_SET_IMAGE_WALLPAPER" "$body")" || die "set-wallpaper command failed"
    print_reply "$reply"
}

main() {
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --host) HOST="${2:?--host requires a value}"; shift 2 ;;
            --port) PORT="${2:?--port requires a value}"; shift 2 ;;
            --image-port) IMAGE_PORT="${2:?--image-port requires a value}"; shift 2 ;;
            -h|--help) usage; exit 0 ;;
            --) shift; break ;;
            -*) die "unknown global flag $1 (see --help)" ;;
            *) break ;;
        esac
    done

    local cmd="${1:-}"
    [[ -n "$cmd" ]] || { usage; exit 1; }
    shift

    case "$cmd" in
        play) cmd_play "$@" ;;
        pause) cmd_pause "$@" ;;
        resume) cmd_resume "$@" ;;
        stop) cmd_stop "$@" ;;
        seek) cmd_seek "$@" ;;
        volume) cmd_volume "$@" ;;
        ping) cmd_ping "$@" ;;
        queue-request) cmd_queue_request "$@" ;;
        queue-next) cmd_queue_next "$@" ;;
        queue-prev) cmd_queue_prev "$@" ;;
        queue-clear) cmd_queue_clear "$@" ;;
        queue-jump) cmd_queue_jump "$@" ;;
        remote) cmd_remote "$@" ;;
        upload-image) cmd_upload_image "$@" ;;
        set-wallpaper) cmd_set_wallpaper "$@" ;;
        help) usage ;;
        *) die "unknown command '$cmd' (see --help)" ;;
    esac
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
    main "$@"
fi
