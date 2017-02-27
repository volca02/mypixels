Mypixels
========

![Alt text](https://cloud.githubusercontent.com/assets/1075171/14241115/1e141884-fa4a-11e5-8724-95decd9ed17d.png "Screenshot")

Mypixels is a very simple terminal emulator written in python using python gtk bindings and python vte bindings.

Main design principles:

* screen space effectivity (no menus/status lines that can't be hidden or take up too much space).
* ease of use
* all keyboard control (the second this readme is written, even tabs can't be switched using mouse).

Some cool features:

* Term tab colorization - mark your terms with a certain colour so it can be easily found
* entanglement of terms - write in numerous terms at once
* all-keyboard operation

Installation
============

You will need python2. Also install python bindings for gtk, vte and pango. Install python-yaml. That should be it.

Configuration
=============

All config values can be changed via ~/.mypixels file (Yaml). Example:

```
background_color: '#101010'
bg_tint_color: '#101010'
default_tab_title: ''
focus_new_tabs: true
foreground_color: '#BBB'
hide_single_tab: true
multiline_tabs: true
palette: '#2E2E34343636:#CCCC00000000:#4E4E9A9A0606:#C4C4A0A00000:#34346565A4A4:#757550507B7B:#060698209A9A:#D3D3D7D7CFCF:#555557575353:#EFEF29292929:#8A8AE2E23434:#FCFCE9E94F4F:#72729F9FCFCF:#ADAD7F7FA8A8:#3434E2E2E2E2:#EEEEEEEEECEC'
scrollback_lines: 1000
tab_color_bg: '#101010'
tab_color_bg_selected: '#303020'
tab_color_fg: '#888'
tab_color_fg_selected: '#fff'
tab_title_max_length: 16
tab_title_short_current: 32
tablist_background: '#101010'
tablist_font: Share Tech 10
#tablist_font: Envy Core R 8
tablist_top: true
term_font: Terminus 10
term_opacity: 0.10
```


Keyboard shortcuts
==================

* M-Left - Prev. tab
* M-SHIFT-f - Prev. tab
* M-Right - Next tab
* M-SHIFT-b - Next. tab
* M-Home - Switch to first tab
* M-End - Switch to last tab
* M-0 - M-9 - Switch to Nth tab

* M-Shift-Left - Move tab left
* M-Shift-Right - Move tab right
* M-Shift-Home - Move tab to begining
* M-Shift-End - Move tab to end

* C-M-Z - Open a new tab
* C-M-X - Close current tab

* M-SHIFT-[ - Next color for current tab
* M-SHIFT-] - Prev color for current tab
* C-SHIFT-v - Toggle tab bar visibility
* M-SHIFT-[Keypad +] - Enlarge font
* M-SHIFT-[Keypad -] - Shrink font

* M-SHIFT-[Keypad *] - Toggle tab entanglement for current tab

* M-SHIFT-n - Set a custom tab name

* M-SHIFT-s - Search for the given text
* M-SHIFT-a - Skip to previous instance of the search
* M-SHIFT-d - Skip to next instance of the search

Keyboard shortcuts are configurable. Example config:

```
bindings:
  close_tab: !!python/tuple
  - CONTROL+ALT+x
  - CONTROL+ALT+q
  enlarge_font: MOD1+SHIFT+KP_Add
  first_tab: MOD1+Home
  goto_tab_0: MOD1+_1
  goto_tab_1: MOD1+_2
  goto_tab_2: MOD1+_3
  goto_tab_3: MOD1+_4
  goto_tab_4: MOD1+_5
  goto_tab_5: MOD1+_6
  goto_tab_6: MOD1+_7
  goto_tab_7: MOD1+_8
  goto_tab_8: MOD1+_9
  goto_tab_9: MOD1+_0
  last_tab: MOD1+End
  move_first_tab: MOD1+SHIFT+Home
  move_last_tab: MOD1+SHIFT+End
  move_next_tab: MOD1+SHIFT+Right
  move_prev_tab: MOD1+SHIFT+Left
  new_tab: !!python/tuple
  - CONTROL+ALT+z
  - CONTROL+SHIFT+t
  next_color: MOD1+SHIFT+braceright
  next_tab: !!python/tuple
  - MOD1+Right
  - MOD1+SHIFT+b
  prev_color: MOD1+SHIFT+braceleft
  prev_tab: !!python/tuple
  - MOD1+Left
  - MOD1+SHIFT+f
  rename_tab: MOD1+SHIFT+n
  shrink_font: MOD1+SHIFT+KP_Subtract
  toggle_entangle: MOD1+SHIFT+KP_Multiply
  toggle_hide_tabs: CONTROL+SHIFT+v
```
