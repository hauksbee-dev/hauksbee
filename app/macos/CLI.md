# Command-line tools from Hauksbee.app

Hauksbee.app keeps its command-line tools under
`Contents/Resources/bin`; the app does not put them on `PATH` automatically.

After copying the app, run the bundled helper (for example):

```sh
/Applications/Hauksbee.app/Contents/Resources/install-cli.sh
```

The default is a user-owned copy in
`~/Library/Application Support/Hauksbee/bin`. It needs no `sudo` and changes
no shell startup file. The helper prints an explicit `PATH` command for the
current shell; add that command to your profile yourself if you want it on
future shells. Use `--prefix DIR` for another absolute, user-owned prefix.

The default copies continue to work if the app is moved, but run the helper
again after installing an app update. `--symlink` is an opt-in alternative;
those links follow the app's current location and break if it is moved or
replaced.

For a private-beta package, the binaries are for the authorised recipient
under `BETA-LICENSE`. Do not redistribute the app or installed command-line
copies.
