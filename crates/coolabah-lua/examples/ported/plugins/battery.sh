#!/bin/sh

# Ported from sketchybar/plugins/battery.sh, driving coolabah's own CLI instead
# of sketchybar's, and reading $COOLABAH_NAME instead of $NAME.
#
# TODO(coolabah): there is no native battery-percentage source/event (see
# crates/protocol/src/event.rs's `events!` list -- volume, brightness and
# power *source* all have one, battery percentage does not), so this stays
# a polling script on `update_freq`, same as the original.
#
# TODO(coolabah): the original also toggles `label.drawing` on `mouse.entered`/
# `mouse.exited` so the percentage only shows on hover. `ItemPatch` has one
# `drawing` flag for the whole item, not one per icon/label, so that
# independent toggle is dropped here -- the label is always shown.

PERCENTAGE="$(pmset -g batt | grep -Eo "\d+%" | cut -d% -f1)"
CHARGING="$(pmset -g batt | grep 'AC Power')"

if [ "$PERCENTAGE" = "" ]; then
  exit 0
fi

case "${PERCENTAGE}" in
  9[0-9]|100) ICON="􀛨"
  ;;
  [6-8][0-9]) ICON="􀺸"
  ;;
  [3-5][0-9]) ICON="􀺶"
  ;;
  [1-2][0-9]) ICON="􀛩"
  ;;
  *) ICON="􀛪"
esac

if [ -n "$CHARGING" ]; then
  ICON=""
fi

coolabah item set "$COOLABAH_NAME" --icon "$ICON" --label "${PERCENTAGE}%"
