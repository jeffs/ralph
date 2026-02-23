#!/usr/bin/env nu

viddy --no-status --no-title --shell nu '$env.config.use_ansi_coloring = true; ralph status --json | from json | where phase != Done and ($it | get -o postmortem) == null | select id title phase phase_ordinal blocked_by attempts | rename index | update blocked_by {str join " "} | update phase_ordinal {0 - $in} | sort-by phase_ordinal blocked_by | reject phase_ordinal'
