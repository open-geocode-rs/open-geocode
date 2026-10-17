# Changelog

## [0.2.0](https://github.com/open-geocode-rs/open-geocode/compare/open-geocode-v0.1.0...open-geocode-v0.2.0) (2026-10-17)


### Features

* add named POI layer ([ef8e090](https://github.com/open-geocode-rs/open-geocode/commit/ef8e0900675b4a9e3af1c3ddc4d112a4a799ab31))
* **api:** answer search and reverse with one GeoJSON contract ([3d6ed23](https://github.com/open-geocode-rs/open-geocode/commit/3d6ed23fcd12c691a1cae1bbbb1c339693dd6574))
* **api:** publish the API contract as OpenAPI ([#21](https://github.com/open-geocode-rs/open-geocode/issues/21)) ([355def0](https://github.com/open-geocode-rs/open-geocode/commit/355def0dae2f3295bbbed2f19a8792caa025314b))
* **bench:** score forward geocoding against G-NAF coordinates ([3f36c35](https://github.com/open-geocode-rs/open-geocode/commit/3f36c359ea0cf72485cf05cfb18dc9fc01639a5f))
* **build:** assign admin layers from libpostal's admin level table ([0aaaf57](https://github.com/open-geocode-rs/open-geocode/commit/0aaaf57157299d51be178641a091942d51ff97c1))
* **builder:** index every name and accept Japanese block addresses ([8a6258b](https://github.com/open-geocode-rs/open-geocode/commit/8a6258b3c7f4b18af7175b9f662b5054bc7f4d1a))
* **builder:** name features by the Latin form of kana, Cyrillic and Greek names ([2ee605b](https://github.com/open-geocode-rs/open-geocode/commit/2ee605bdc04ece707f027277528096d423a612a7))
* **builder:** read places drawn as areas ([dede286](https://github.com/open-geocode-rs/open-geocode/commit/dede28681a00fea54e8ba007f952dd844e347a2f))
* **build:** train the address parser inside build ([03ef643](https://github.com/open-geocode-rs/open-geocode/commit/03ef643e04591e743f113516a03816b8b27c2eba))
* og, an interactive terminal for Packs, builds, downloads and batch geocoding ([#20](https://github.com/open-geocode-rs/open-geocode/issues/20)) ([b1aeee9](https://github.com/open-geocode-rs/open-geocode/commit/b1aeee948429758be3f24301547471c4dd84f3cf))
* **parse-bench:** split scores by dictionary coverage ([3b84dba](https://github.com/open-geocode-rs/open-geocode/commit/3b84dba1519670ff143060a9c35e31a5570a4636))
* **parse-train:** give block scheme addresses a share of their own ([b357973](https://github.com/open-geocode-rs/open-geocode/commit/b3579738519d999ab10e57e67793c63b07dfb21a))
* **parse-train:** write region and country records as rows of their own ([ea1ea68](https://github.com/open-geocode-rs/open-geocode/commit/ea1ea68d680c88ff04ff1d85337f6e673c58dc32))
* **parse-train:** write the areas an address states as one locality ([92b91dd](https://github.com/open-geocode-rs/open-geocode/commit/92b91dd54419c834666ed2adae3b41dfda529c25))
* **parse:** drop labellings whose places cannot be one address ([f23817e](https://github.com/open-geocode-rs/open-geocode/commit/f23817e473eef10d5909aacfa60172f70861ca31))
* **parse:** drop the district label, as libpostal's data does ([0036e08](https://github.com/open-geocode-rs/open-geocode/commit/0036e08f6b6938b473ea738dc06e73d79339e10c))
* **parse:** ground places and postcodes in what the Pack explains ([4fd119c](https://github.com/open-geocode-rs/open-geocode/commit/4fd119c5804c3b7304a2a8a0c85612584b621f03))
* **parse:** hash the affixes of a span's word into features ([2100a84](https://github.com/open-geocode-rs/open-geocode/commit/2100a843365a0aeee246cd7ba210580975a676bb))
* **parse:** label address tokens with a lattice parser ([f305530](https://github.com/open-geocode-rs/open-geocode/commit/f30553044c4dcb6b57ff469e7f37147dac55ac69))
* **parse:** learn POI names as a dictionary field of their own ([4a0a6de](https://github.com/open-geocode-rs/open-geocode/commit/4a0a6de78438a94135c2bdd84f7b5edf37d50ba3))
* **parse:** learn unit words from the Pack's addr:unit values ([3184505](https://github.com/open-geocode-rs/open-geocode/commit/318450513517a4ddc97eb13f4fd957effd039c03))
* **parse:** learn units, street spellings and category queries ([2269b8c](https://github.com/open-geocode-rs/open-geocode/commit/2269b8cfa53597a26bfe23f22ea295be60d849cf))
* **parse:** name admin areas without their type word ([54fecf0](https://github.com/open-geocode-rs/open-geocode/commit/54fecf08884378ae923eae7b96d69faa93ed3f80))
* **parse:** name the groups of regions a region lies in ([410adc5](https://github.com/open-geocode-rs/open-geocode/commit/410adc590dd08d93c42f95d237fe6faee1483fb3))
* **parse:** read admin type words on the side they are written ([9368170](https://github.com/open-geocode-rs/open-geocode/commit/9368170a500d73c5193830775af8e84f1365ee23))
* **parse:** read districts, glued units, abbreviations and near phrases ([8784ab7](https://github.com/open-geocode-rs/open-geocode/commit/8784ab7efc8e56d4a30d4859a97bd6c660861ce5))
* **parse:** read glued area names as one chain ([9f60dac](https://github.com/open-geocode-rs/open-geocode/commit/9f60dac66f69b4a06e576caaa5e8d978fa20d692))
* **parse:** read numbered areas and check places where the Pack is complete ([880c28a](https://github.com/open-geocode-rs/open-geocode/commit/880c28aedb568921dbae2d8c60eac37b290fa160))
* **parse:** read numerals of any script as digits ([8250e93](https://github.com/open-geocode-rs/open-geocode/commit/8250e93dec5203ba00fa68915ca29485b4e79c3a))
* **parse:** read postcodes after country letters and the dominant field ([3783ca9](https://github.com/open-geocode-rs/open-geocode/commit/3783ca92a827dc6b6bad6cad344094493f027d9b))
* **parse:** read scripts without spaces by the Unicode Script property ([a250543](https://github.com/open-geocode-rs/open-geocode/commit/a25054362a5e269acac9d62b4a384932a63e16e9))
* **parse:** score address parsing against libpostal gold rows ([61114dd](https://github.com/open-geocode-rs/open-geocode/commit/61114dd272248062ad48ff9636ed8628570643f4))
* **parse:** train a semi-Markov CRF with hashed word features ([08bccee](https://github.com/open-geocode-rs/open-geocode/commit/08bccee52d6ea207aa2c263c1d98a1da2cf01dc2))
* **parse:** vendor libpostal's word lists as phrase dictionaries ([dfb0a7e](https://github.com/open-geocode-rs/open-geocode/commit/dfb0a7e758dd0023feac082b2b0b85aae4ed3722))
* **runtime:** serve structured address geocoding at /geocode ([#7](https://github.com/open-geocode-rs/open-geocode/issues/7)) ([37db890](https://github.com/open-geocode-rs/open-geocode/commit/37db8905487d2a452aec54960ad06d17b6874d20))
* score bench fixtures against expected POIs ([2fbb81c](https://github.com/open-geocode-rs/open-geocode/commit/2fbb81cd182de241c1e99d83a5d1708129c38bc3))
* **search:** plan queries from the address parser ([b0f9607](https://github.com/open-geocode-rs/open-geocode/commit/b0f9607f73aa91afb2caf42e4b53d9284be3b0aa))
* stream builds to planet scale with a single-file Pack ([d6b8e85](https://github.com/open-geocode-rs/open-geocode/commit/d6b8e85061e7b94ba7b590c28590c874e5abab6c))


### Bug Fixes

* address review findings on the planet-scale build ([c661eb2](https://github.com/open-geocode-rs/open-geocode/commit/c661eb2791687d966ae7ccaa54f7b68aed97e00d))
* keep every build buffer within the memory budget ([598bd30](https://github.com/open-geocode-rs/open-geocode/commit/598bd30b47ab8fb93448fa613dede56b0c4b3beb))
* **pack:** publish validated generations without replacing live data ([c804e26](https://github.com/open-geocode-rs/open-geocode/commit/c804e26fe9cc7a0049e578ada8ed806168e7c49d))
* **parse:** check admin aliases against the names of areas only ([5575578](https://github.com/open-geocode-rs/open-geocode/commit/55755780e1fd706d302cd005dc910aeb699df574))
* **parse:** ground each closed-class label in a name of its own field ([4c1fa08](https://github.com/open-geocode-rs/open-geocode/commit/4c1fa08fc99ebb54d143b837b426f41bcbbbd093))
* **parse:** read a query as the countries its evidence points to ([58dc4e9](https://github.com/open-geocode-rs/open-geocode/commit/58dc4e964716654a22fca4ca9cf79ee5d5b06e26))
* **postcode:** place a postcode at the medoid of a bounded member sample, not the mean ([9290fd5](https://github.com/open-geocode-rs/open-geocode/commit/9290fd5d396fd3933beb7752d4196c50f86122aa))
* **postcode:** place a postcode at the medoid of a bounded member sample, not the mean ([aa4a593](https://github.com/open-geocode-rs/open-geocode/commit/aa4a593c5e19196226dfc5b12076d709047e6018)), closes [#3](https://github.com/open-geocode-rs/open-geocode/issues/3)
* **resources:** keep the rlimit cast for 32-bit targets ([db77683](https://github.com/open-geocode-rs/open-geocode/commit/db77683f59c3754e2779c30a816c873f7903fd2b))
* **runtime:** log the cause of every internal error under its request id ([816d331](https://github.com/open-geocode-rs/open-geocode/commit/816d331014d8c33e267052967fcc081c2614d893))
* **runtime:** log the cause of every internal error under its request id ([2dfdea8](https://github.com/open-geocode-rs/open-geocode/commit/2dfdea846da178ace2cee97bc95556310f8c9f83)), closes [#2](https://github.com/open-geocode-rs/open-geocode/issues/2)
* **search:** drop hits that contradict a stated postcode ([bbeb739](https://github.com/open-geocode-rs/open-geocode/commit/bbeb739c3e2e3f604c02323564d4afb7e67293bb))
* **search:** put a stand-in street on its own centre line ([43ea64d](https://github.com/open-geocode-rs/open-geocode/commit/43ea64d67dfd83398f69fd65ae9f2932d989f41f))


### Performance Improvements

* bound the string table by budget and parallelize anchor tags ([5f9ba50](https://github.com/open-geocode-rs/open-geocode/commit/5f9ba504ec8206f69845b74ca56376afe47ae4ba))
* **build:** size the build to the host and train in seconds ([e399f49](https://github.com/open-geocode-rs/open-geocode/commit/e399f495475e5ed10069f433dacd3354d1a4ea2f))
* **parse:** decode more labellings only when the best are no address ([bb4c92a](https://github.com/open-geocode-rs/open-geocode/commit/bb4c92a7c993b176dc6a75c6d831007b3f831f68))
* **records:** build summaries without decoding geometry ([034b1e5](https://github.com/open-geocode-rs/open-geocode/commit/034b1e53af2f189bbae3d57baf2d371401199029))
* **spatial:** select nearest candidates before sorting ([88f97d5](https://github.com/open-geocode-rs/open-geocode/commit/88f97d5bbf0a1bee6b62acf53a67df6cd63bdb2a))
