# Several stacks in one checkout

A checkout can hold several stacks, such as `-p shop-test` next to the default
one. Each is named by its Compose project name.

```bash
ulak status              # every stack declared here, side by side
ulak status shop-test    # one of them, from any directory
ulak sync shop-test
ulak doctor shop-test
ulak clean shop-test
```

Without a name, `doctor` and `sync` ask which one; outside a terminal they
list one command per stack instead. `clean` always needs the name. Write
`name@server` when two servers have a stack of that name.
