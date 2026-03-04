## [0.14.0] - 2026-03-04

### 🚀 Features

- Add hostname support to Backend and Builder structs
- Implement multi-writer support in Logger for multiple appenders

### 🐛 Bug Fixes

- Change duration buckets to fastedge request duration metrics
- Remove NullAppender from Logger initialization in http.rs
- Remove NullAppender from Logger initialization in http.rs

### ⚙️ Miscellaneous Tasks

- Update dependencies in Cargo.lock to latest versions
## [0.13.5] - 2026-01-09

### 🐛 Bug Fixes

- Set default plan_id field in app configuration struct

### ⚙️ Miscellaneous Tasks

- Release
## [0.13.4] - 2026-01-06

### 🚀 Features

- Add stats tracking to HTTP execution context
- Simplify method signatures and improve executor property handling
- Update request ID handling and stats row creation to use SmolStr references
- Add stats tracking and new fields to HTTP state and key-value store
- Refactor dictionary module to utils and implement UserDiagStats trait
- Add external request stats tracking and implement ExtRequestStats trait
- Refactor external request stats handling and update WasiHttpView trait
- Clean up code formatting and improve readability in various modules
- Improve code readability in open function by adjusting formatting
- Remove unnecessary instrumentation from backend_request method

### 🐛 Bug Fixes

- Update unit test handle_request calls to use SmolStr references
- Add public host validation to prevent private host usage
- Add public host validation and extract host function; downgrade package versions to 0.13.0-2
- Update private IP address checks
- Improve readability of error assertions in request tests
- Reduce info logging level to debug for cache addition and request processing
- Change logging level from debug to info for cache addition

### ⚙️ Miscellaneous Tasks

- Update wit hash to reflect latest changes
- Release
- Release
## [0.13.2] - 2025-11-04

### 🐛 Bug Fixes

- Update Redis dependency to version 0.32 and improve error handling in async item retrieval

### ⚙️ Miscellaneous Tasks

- Release
## [0.13.1] - 2025-11-04

### 🐛 Bug Fixes

- Default no value for kv store param
- Update wit path in component bindgen and add submodule configuration

### 🚜 Refactor

- Rename zrange to zrange_by_score for clarity and update return type

### ⚙️ Miscellaneous Tasks

- Release
## [0.13.0] - 2025-10-09

### 💼 Other

- Proxy-wasm dictionary interface and sdk v0.3.0

### ⚙️ Miscellaneous Tasks

- Release
## [0.12.1] - 2025-10-03

### 🐛 Bug Fixes

- Replace cuckoo filter with bloom

### ⚙️ Miscellaneous Tasks

- Release
## [0.12.0] - 2025-09-24

### 🐛 Bug Fixes

- Added fossa CI workflow
- Use_tls for https scheme

### ⚙️ Miscellaneous Tasks

- Release
## [0.11.10-rc.2] - 2025-09-18

### 🐛 Bug Fixes

- Make pub new kv store methods and forward sdk to proxywasm changes

### ⚙️ Miscellaneous Tasks

- Release
## [0.11.10-rc.1] - 2025-09-18

### 🚀 Features

- Updating wasmtime to 36.0.0
- Updating key-value store interface
- Adding redis store impl

### 🐛 Bug Fixes

- Formating
- Changing score type to f64
- Formatting

### 🚜 Refactor

- Generic store type

### ⚙️ Miscellaneous Tasks

- Release
## [0.11.9] - 2025-08-29

### 🐛 Bug Fixes

- Use direct backend for fastedge cli
- Compilation errors

### ⚙️ Miscellaneous Tasks

- Release
## [0.11.8] - 2025-06-05

### ⚙️ Miscellaneous Tasks

- Release
## [0.11.7] - 2025-04-25

### 🚀 Features

- Adding WasmConfig builder with max execution_stacks parameter

### ⚙️ Miscellaneous Tasks

- Release
## [0.11.6] - 2025-04-04

### 🐛 Bug Fixes

- Backend request authority

### ⚙️ Miscellaneous Tasks

- Release
## [0.11.5] - 2025-04-03

### 🐛 Bug Fixes

- Mapping store name with db url

### ⚙️ Miscellaneous Tasks

- Release
## [0.11.4] - 2025-04-02

### 🚜 Refactor

- Updating wasmtime to 31.0.0

### ⚙️ Miscellaneous Tasks

- Release
## [0.11.3] - 2025-04-02

### 🐛 Bug Fixes

- Updating wasmtime-wasi-nn to latest candle

### ⚙️ Miscellaneous Tasks

- Release
## [0.11.2] - 2025-04-02

### 🐛 Bug Fixes

- Updating prometheus dependency to fix protobuf licence issue

### ⚙️ Miscellaneous Tasks

- Release
## [0.11.1] - 2025-04-01

### 🐛 Bug Fixes

- Fixing catching close signal on windows platform

### ⚙️ Miscellaneous Tasks

- Release
## [0.11.0] - 2025-04-01

### 🐛 Bug Fixes

- Fastedge-run handler for terminate signal
- Updated audit-check version
- Disabling cargo audit
- Setting use_tls to false in OutgoingRequestConfig

### ⚙️ Miscellaneous Tasks

- Release
## [0.10.2] - 2025-02-21

### 🐛 Bug Fixes

- Wasm app logging

### ⚙️ Miscellaneous Tasks

- Release
## [0.10.1] - 2025-02-20

### 🐛 Bug Fixes

- Log backend send error and fixing unit tests
- Mixing of http scheme and cloning of InstancePre obj

### 🚜 Refactor

- StatRow structure

### ⚙️ Miscellaneous Tasks

- Release
## [0.10.0] - 2025-02-19

### 🚜 Refactor

- Wasmtime update
- Updating wasmtime dependency to 29.0.1

### ⚙️ Miscellaneous Tasks

- Release
## [0.9.4] - 2025-01-21

### 🐛 Bug Fixes

- Use ubuntu-22.04 as runner for cli to avoid glibc not found error
- Change cli tool name to fastedge-run

### ⚙️ Miscellaneous Tasks

- Release
## [0.9.3] - 2025-01-20

### 🐛 Bug Fixes

- Graceful shutdown for http service
- Incorrect stats written for failing wasi-http app
- Shellflip conditional compilation for non unix platform
- Windows compilation for unused variable

### ⚙️ Miscellaneous Tasks

- Release
## [0.9.2] - 2024-12-18

### 🚀 Features

- Adding secret get_effective_at method host implementation

### ⚙️ Miscellaneous Tasks

- Release
## [0.9.1] - 2024-12-06

### 🐛 Bug Fixes

- Remove usage of CI rust cache
- Raise and alert for secret decryption errors
- Set Fastedge_Header_Hostname header for backend

### ⚙️ Miscellaneous Tasks

- Release
## [0.9.0] - 2024-11-22

### 🐛 Bug Fixes

- Process http body chunks
- Adding on_response handler to process the end of body chunks
- Moving unit test to http sub-module

### ⚙️ Miscellaneous Tasks

- Release
## [0.8.1] - 2024-11-11

### ⚙️ Miscellaneous Tasks

- Release
## [0.8.0] - 2024-10-21

### 🚀 Features

- Impl draft secret libs
- Secret common lib implementation

### 🐛 Bug Fixes

- Minor clippy error fix

### ⚙️ Miscellaneous Tasks

- Release
## [0.7.0] - 2024-10-16

### 🚜 Refactor

- Drop async from write_stats method

### ⚙️ Miscellaneous Tasks

- Release
## [0.6.0] - 2024-09-20

### 🐛 Bug Fixes

- Stopping execution of blocking wasm code on timeout
- Formatting and handle wasi-http timeouts

### ⚙️ Miscellaneous Tasks

- Release
## [0.5.7] - 2024-09-12

### 🐛 Bug Fixes

- Adding support for cargo release and auto PR on new release
- Adding release process description
- Formatting check
- Setting prerelease flag for release candidates

### ⚙️ Miscellaneous Tasks

- Release
## [0.5.7-rc.2] - 2024-09-09

### 🐛 Bug Fixes

- Release pipeline

### ⚙️ Miscellaneous Tasks

- Release
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

### ⚙️ Miscellaneous Tasks

- Release
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
