# Wasm Interface Type (WIT)

This directory is synced from the upstream `FastEdge-wit` repository with [git subtree](https://www.atlassian.com/git/tutorials/git-subtree). That keeps the WIT files available directly in this repository while still allowing updates from the upstream source.

To add the WIT definitions to another repository as a subtree, run:

```sh
git subtree add --prefix=<path> https://github.com/G-Core/FastEdge-wit.git main --squash
```

To refresh an existing subtree from upstream, run:

```sh
git subtree pull --prefix=<path> https://github.com/G-Core/FastEdge-wit.git main --squash
```
