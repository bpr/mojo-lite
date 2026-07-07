# `count = 10` on an undeclared name is a var-less variable introduction (implicit
# declaration). The compiler backend runs it as an ordinary binding — the same as
# `var count = 10` (the old tree-walker deferred it as "parse now, run later").
count = 10
print(count)
