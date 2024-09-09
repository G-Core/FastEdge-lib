# Changelog

All notable changes to this project will be documented in this file.

## [0.5.7-rc.1] - 2024-09-09

### 🚀 Features

- Refactored ExecutorCache trait
- Fix version to Preview2
- Adding request duration and wasm memory used metrics

### 🐛 Bug Fixes

- Set by default total core instance
- Adding requestor field for wasi-http
- Request duration metric
- Change wasm memory usage metric type
- Print execution error and set proper process status code on error
- Change release pipeline
- Adding cargo realease settings

## [0.5.2-3] - 2024-08-05

### 🐛 Bug Fixes

- Add os target

## [0.5.2-2] - 2024-08-05

### 🐛 Bug Fixes

- Add os target

## [0.5.2-1] - 2024-08-05

### 🐛 Bug Fixes

- Simplify tag creation and trigger release on push tag

## [0.5.2] - 2024-08-05

### 🐛 Bug Fixes

- Add server_name as local request authority and remove default http/https port
- Parsing envs and headers arg
- Changed hyper::Error to anyhow::Error
- Adding tag creation
- Adding tag creation
- Adding tag creation
- Adding tag creation
- Adding tag creation
- Adding tag creation
- Adding tag creation
- Drop windows from package list
- Release tag name
- Simplify tag creation and trigger release on push tag

## [0.5.0] - 2024-07-30

### 🚀 Features

- Adding support for graceful shutdown
- Adding cli support for wasi-http
- Update hyper deps to 1.4
- Add support for WASI HTTP interface
- Write request_id to clickhouse stats

### 🐛 Bug Fixes

- Remove unusual reference
- Add uri missing schema part

## [0.4.1] - 2024-06-26

### 🚀 Features

- Return custom error codes for internal fastedge errors
- Adding region field to stats and minor string fields perf optimisations

### 🐛 Bug Fixes

- Set by default 60s for max_duration cli parameter
- Add alloc fixture for unit tests
- Add alloc fixture for unit tests
- Adding app lookup by id trait
- Comment code coverage step
- Refactoring stats and metric sub modules
- Fix github pipeline
- Fix github release pipeline
- Add pipeline caching
- Add pipeline caching
- Release pipeline
- Release pipeline
- Make tls as optional http-service feature
- Small app log improvements

### ⚙️ Miscellaneous Tasks

- Release

## [0.3.7] - 2024-05-15

### 🚀 Features

- Adding matrix release for different platforms

### 🐛 Bug Fixes

- Clippy warning and new release flow
- Clippy warning and new release flow
- Clippy warning and new release flow

<!-- generated by git-cliff -->