# Changelog

## [0.2.0](https://github.com/open-geocode-rs/open-geocode/compare/open-geocode-v0.1.0...open-geocode-v0.2.0) (2026-10-09)


### Features

* stream builds to planet scale with a single-file Pack ([d6b8e85](https://github.com/open-geocode-rs/open-geocode/commit/d6b8e85061e7b94ba7b590c28590c874e5abab6c))


### Bug Fixes

* address review findings on the planet-scale build ([c661eb2](https://github.com/open-geocode-rs/open-geocode/commit/c661eb2791687d966ae7ccaa54f7b68aed97e00d))
* keep every build buffer within the memory budget ([598bd30](https://github.com/open-geocode-rs/open-geocode/commit/598bd30b47ab8fb93448fa613dede56b0c4b3beb))
* **pack:** publish validated generations without replacing live data ([c804e26](https://github.com/open-geocode-rs/open-geocode/commit/c804e26fe9cc7a0049e578ada8ed806168e7c49d))


### Performance Improvements

* bound the string table by budget and parallelize anchor tags ([5f9ba50](https://github.com/open-geocode-rs/open-geocode/commit/5f9ba504ec8206f69845b74ca56376afe47ae4ba))
* **records:** build summaries without decoding geometry ([034b1e5](https://github.com/open-geocode-rs/open-geocode/commit/034b1e53af2f189bbae3d57baf2d371401199029))
* **spatial:** select nearest candidates before sorting ([88f97d5](https://github.com/open-geocode-rs/open-geocode/commit/88f97d5bbf0a1bee6b62acf53a67df6cd63bdb2a))
