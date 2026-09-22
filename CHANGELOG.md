# Changelog

## [0.18.0](https://github.com/MnlPhlp/tauri-plugin-blec/compare/v0.17.0...v0.18.0) (2026-09-22)


### ⚠ BREAKING CHANGES

* `check_permissions` is async now, and

### Features

* add connected device mtu getter ([#52](https://github.com/MnlPhlp/tauri-plugin-blec/issues/52)) ([d29ae2f](https://github.com/MnlPhlp/tauri-plugin-blec/commit/d29ae2f3467e4807cd8ceed6fdd7376895cf8d4f))
* Expose listServices to JS api ([#33](https://github.com/MnlPhlp/tauri-plugin-blec/issues/33)) ([415b613](https://github.com/MnlPhlp/tauri-plugin-blec/commit/415b613f887ca704b7f1da3c21648185aa8fde0c))
* split the BLE client out of the Tauri plugin and add Dioxus support ([#62](https://github.com/MnlPhlp/tauri-plugin-blec/issues/62)) ([d882cc1](https://github.com/MnlPhlp/tauri-plugin-blec/commit/d882cc1b9dacca2ace0147d5b75cb9f11bbaa807))


### Bug Fixes

* **android:** don't drop scanned devices when is_bonded() fails ([#55](https://github.com/MnlPhlp/tauri-plugin-blec/issues/55)) ([fbade6c](https://github.com/MnlPhlp/tauri-plugin-blec/commit/fbade6ca37941b53d26c0cfebe39ccbf37ff8bc2))
* **android:** notify channel is never fired on android 12 and below ([#44](https://github.com/MnlPhlp/tauri-plugin-blec/issues/44)) ([096bb05](https://github.com/MnlPhlp/tauri-plugin-blec/commit/096bb051cf745d55e6bfff5f27cebc2199a11a5d))
* avoid holding mutex locks across .await in BLE handler ([#39](https://github.com/MnlPhlp/tauri-plugin-blec/issues/39)) ([7d02f6f](https://github.com/MnlPhlp/tauri-plugin-blec/commit/7d02f6f49a75ff7a298f7f181b9e60369c3477cc))
* **commands:** change channel size ([#57](https://github.com/MnlPhlp/tauri-plugin-blec/issues/57)) ([031da6d](https://github.com/MnlPhlp/tauri-plugin-blec/commit/031da6d233aee3bdb559453a39be646677fe3252))
* Use actual characteristic properties in ResCharacteristic ([#40](https://github.com/MnlPhlp/tauri-plugin-blec/issues/40)) ([6a09ef3](https://github.com/MnlPhlp/tauri-plugin-blec/commit/6a09ef3a96901b58b32f71360a3fb3b92bb6fe98))


### Performance Improvements

* add try_init ([#45](https://github.com/MnlPhlp/tauri-plugin-blec/issues/45)) ([a91f4e8](https://github.com/MnlPhlp/tauri-plugin-blec/commit/a91f4e8fcb948eb0087144ba1522cc673b10055a))

## [0.17.0](https://github.com/MnlPhlp/tauri-plugin-blec/compare/v0.16.0...v0.17.0) (2026-09-22)


### ⚠ BREAKING CHANGES

* `check_permissions` is async now, and

### Features

* add connected device mtu getter ([#52](https://github.com/MnlPhlp/tauri-plugin-blec/issues/52)) ([d29ae2f](https://github.com/MnlPhlp/tauri-plugin-blec/commit/d29ae2f3467e4807cd8ceed6fdd7376895cf8d4f))
* Expose listServices to JS api ([#33](https://github.com/MnlPhlp/tauri-plugin-blec/issues/33)) ([415b613](https://github.com/MnlPhlp/tauri-plugin-blec/commit/415b613f887ca704b7f1da3c21648185aa8fde0c))
* split the BLE client out of the Tauri plugin and add Dioxus support ([#62](https://github.com/MnlPhlp/tauri-plugin-blec/issues/62)) ([d882cc1](https://github.com/MnlPhlp/tauri-plugin-blec/commit/d882cc1b9dacca2ace0147d5b75cb9f11bbaa807))


### Bug Fixes

* **android:** don't drop scanned devices when is_bonded() fails ([#55](https://github.com/MnlPhlp/tauri-plugin-blec/issues/55)) ([fbade6c](https://github.com/MnlPhlp/tauri-plugin-blec/commit/fbade6ca37941b53d26c0cfebe39ccbf37ff8bc2))
* **android:** notify channel is never fired on android 12 and below ([#44](https://github.com/MnlPhlp/tauri-plugin-blec/issues/44)) ([096bb05](https://github.com/MnlPhlp/tauri-plugin-blec/commit/096bb051cf745d55e6bfff5f27cebc2199a11a5d))
* avoid holding mutex locks across .await in BLE handler ([#39](https://github.com/MnlPhlp/tauri-plugin-blec/issues/39)) ([7d02f6f](https://github.com/MnlPhlp/tauri-plugin-blec/commit/7d02f6f49a75ff7a298f7f181b9e60369c3477cc))
* **commands:** change channel size ([#57](https://github.com/MnlPhlp/tauri-plugin-blec/issues/57)) ([031da6d](https://github.com/MnlPhlp/tauri-plugin-blec/commit/031da6d233aee3bdb559453a39be646677fe3252))
* Use actual characteristic properties in ResCharacteristic ([#40](https://github.com/MnlPhlp/tauri-plugin-blec/issues/40)) ([6a09ef3](https://github.com/MnlPhlp/tauri-plugin-blec/commit/6a09ef3a96901b58b32f71360a3fb3b92bb6fe98))


### Performance Improvements

* add try_init ([#45](https://github.com/MnlPhlp/tauri-plugin-blec/issues/45)) ([a91f4e8](https://github.com/MnlPhlp/tauri-plugin-blec/commit/a91f4e8fcb948eb0087144ba1522cc673b10055a))
