#!/usr/bin/env nu
 
viddy --no-status --no-title 'jj --color=always status | rg -v "^[WP]"; print ""; jj --color=always'
