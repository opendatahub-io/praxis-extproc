[.items[].endpoints[]? | select(.conditions.ready == true and (.addresses | length) > 0)] as $ready |
($ready | length) == 1 and any(.items[]?.ports[]?; .port == 443)
