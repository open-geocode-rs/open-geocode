# Changelog

## [1.0.0](https://github.com/open-geocode-rs/open-geocode/compare/open-geocode-v0.1.0...open-geocode-v1.0.0) (2026-10-23)


### ⚠ BREAKING CHANGES

* **search:** rate each answer with a calibrated confidence
* **build:** keep one record of a house two sources place apart
* **search:** weigh a postcode by how few addresses it covers in its country
* **search:** fold Latin letters to ASCII in search keys
* **postcode:** key postcodes by country and lend a street's postcodes
* **address:** read slash house numbers from their street
* **search:** index one text document per street group
* **spatial:** index record blocks and lines in packed Hilbert R-trees
* **records:** store each street's addresses as one run
* **context:** let one area be several tiers at once
* **context:** rank admin areas into a chain, not six slots
* **search:** TextSearchOptions, TextAutocompleteOptions and AddressGeocodeOptions take a `countries` field.
* **build:** BuildOsmOptions takes `inputs` and `input_sources` rather than `input` and `input_source`. The build report replaces `input`, `input_source` and `input_replication_timestamp` with an `inputs` array, schema version 19. `og build` and `pack build` take `--region` and `--input` more than once.
* **error:** public functions return open_geocode::Error rather than anyhow::Error.

### Features

* add named POI layer ([ef8e090](https://github.com/open-geocode-rs/open-geocode/commit/ef8e0900675b4a9e3af1c3ddc4d112a4a799ab31))
* **api:** answer search and reverse with one GeoJSON contract ([3d6ed23](https://github.com/open-geocode-rs/open-geocode/commit/3d6ed23fcd12c691a1cae1bbbb1c339693dd6574))
* **api:** publish the API contract as OpenAPI ([#21](https://github.com/open-geocode-rs/open-geocode/issues/21)) ([355def0](https://github.com/open-geocode-rs/open-geocode/commit/355def0dae2f3295bbbed2f19a8792caa025314b))
* **bench:** score forward geocoding against G-NAF coordinates ([3f36c35](https://github.com/open-geocode-rs/open-geocode/commit/3f36c359ea0cf72485cf05cfb18dc9fc01639a5f))
* **build:** assign admin layers from libpostal's admin level table ([0aaaf57](https://github.com/open-geocode-rs/open-geocode/commit/0aaaf57157299d51be178641a091942d51ff97c1))
* **build:** build one Pack from several extracts ([94d539b](https://github.com/open-geocode-rs/open-geocode/commit/94d539bbffae91566018390e9a158253f6dcd664))
* **build:** compress and leash scratch, merge runs in one pass, and report what each stage cost ([b119813](https://github.com/open-geocode-rs/open-geocode/commit/b11981376555f10c1426b8f4a784e631e0f83641))
* **builder:** add OpenAddresses as optional address enrichment ([fd431fb](https://github.com/open-geocode-rs/open-geocode/commit/fd431fb0608cc74f3aef84ff296f1624be97488a))
* **builder:** index every name and accept Japanese block addresses ([8a6258b](https://github.com/open-geocode-rs/open-geocode/commit/8a6258b3c7f4b18af7175b9f662b5054bc7f4d1a))
* **builder:** name features by the Latin form of kana, Cyrillic and Greek names ([2ee605b](https://github.com/open-geocode-rs/open-geocode/commit/2ee605bdc04ece707f027277528096d423a612a7))
* **builder:** read places drawn as areas ([dede286](https://github.com/open-geocode-rs/open-geocode/commit/dede28681a00fea54e8ba007f952dd844e347a2f))
* **build:** go on with a stopped Pack build from its last checkpoint ([8b3f3c8](https://github.com/open-geocode-rs/open-geocode/commit/8b3f3c85585e3e089a81f5548de665a5214c503c))
* **build:** join the ways of a street into one record named by its town ([cf5c19f](https://github.com/open-geocode-rs/open-geocode/commit/cf5c19fdd7c65f67b5d5221f6be5af008751076a))
* **build:** keep one record of a house two sources place apart ([f872c14](https://github.com/open-geocode-rs/open-geocode/commit/f872c14fd604656ac12d0e7079c11adfd0791690))
* **build:** train the address parser inside build ([03ef643](https://github.com/open-geocode-rs/open-geocode/commit/03ef643e04591e743f113516a03816b8b27c2eba))
* **context:** let one area be several tiers at once ([bc08aa5](https://github.com/open-geocode-rs/open-geocode/commit/bc08aa54de181081ff7492c804653f7562c3265f))
* **context:** rank admin areas into a chain, not six slots ([a4b18e2](https://github.com/open-geocode-rs/open-geocode/commit/a4b18e241e24ded75b95dc4d49df771435a98e4f))
* **error:** return a typed Error from the geocoding API ([#23](https://github.com/open-geocode-rs/open-geocode/issues/23)) ([c293e0c](https://github.com/open-geocode-rs/open-geocode/commit/c293e0cee37f9d5b2f756e564e7a1c61c020094e))
* **extsort:** add SortStats::merge ([bc91cdd](https://github.com/open-geocode-rs/open-geocode/commit/bc91cdd8c5da9231ee896fa78d073964ea725045))
* **index:** index the country each record lies in ([62f2bc3](https://github.com/open-geocode-rs/open-geocode/commit/62f2bc3040c8c4b6ea68aa2aa0f83a02cf651f96))
* og, an interactive terminal for Packs, builds, downloads and batch geocoding ([#20](https://github.com/open-geocode-rs/open-geocode/issues/20)) ([b1aeee9](https://github.com/open-geocode-rs/open-geocode/commit/b1aeee948429758be3f24301547471c4dd84f3cf))
* **og:** offer OpenAddresses on the confirm screen ([18b0b29](https://github.com/open-geocode-rs/open-geocode/commit/18b0b2940d05c467ee80f77c669f7d519d64d522))
* **parse-bench:** split scores by dictionary coverage ([3b84dba](https://github.com/open-geocode-rs/open-geocode/commit/3b84dba1519670ff143060a9c35e31a5570a4636))
* **parse-train:** give block scheme addresses a share of their own ([b357973](https://github.com/open-geocode-rs/open-geocode/commit/b3579738519d999ab10e57e67793c63b07dfb21a))
* **parse-train:** write region and country records as rows of their own ([ea1ea68](https://github.com/open-geocode-rs/open-geocode/commit/ea1ea68d680c88ff04ff1d85337f6e673c58dc32))
* **parse-train:** write the areas an address states as one locality ([92b91dd](https://github.com/open-geocode-rs/open-geocode/commit/92b91dd54419c834666ed2adae3b41dfda529c25))
* **parse:** drop labellings whose places cannot be one address ([f23817e](https://github.com/open-geocode-rs/open-geocode/commit/f23817e473eef10d5909aacfa60172f70861ca31))
* **parse:** drop the district label, as libpostal's data does ([0036e08](https://github.com/open-geocode-rs/open-geocode/commit/0036e08f6b6938b473ea738dc06e73d79339e10c))
* **parse:** ground places and postcodes in what the Pack explains ([4fd119c](https://github.com/open-geocode-rs/open-geocode/commit/4fd119c5804c3b7304a2a8a0c85612584b621f03))
* **parse:** hash the affixes of a span's word into features ([2100a84](https://github.com/open-geocode-rs/open-geocode/commit/2100a843365a0aeee246cd7ba210580975a676bb))
* **parse:** keep the name a misspelt span was read as ([b3502f1](https://github.com/open-geocode-rs/open-geocode/commit/b3502f165d36138d5b39f8d133d0ec1a57d46cab))
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
* **postcode:** key postcodes by country and lend a street's postcodes ([8efbe3b](https://github.com/open-geocode-rs/open-geocode/commit/8efbe3b91f951301ccfac4c6e5f292d4046cbfc7))
* **records:** store each street's addresses as one run ([bafb02b](https://github.com/open-geocode-rs/open-geocode/commit/bafb02b1be365ecbe53387714d68206a2a80442b))
* **runtime:** serve structured address geocoding at /geocode ([#7](https://github.com/open-geocode-rs/open-geocode/issues/7)) ([37db890](https://github.com/open-geocode-rs/open-geocode/commit/37db8905487d2a452aec54960ad06d17b6874d20))
* score bench fixtures against expected POIs ([2fbb81c](https://github.com/open-geocode-rs/open-geocode/commit/2fbb81cd182de241c1e99d83a5d1708129c38bc3))
* **search:** answer messy web addresses: countries, regions, postal towns, noise and fallbacks ([db6e5e2](https://github.com/open-geocode-rs/open-geocode/commit/db6e5e25a484d138c2e6823cf79a9ecb44c525c4))
* **search:** break equal-name ties toward the larger place ([2602916](https://github.com/open-geocode-rs/open-geocode/commit/260291694349de2233e51937847b07d6c12f09dc))
* **search:** break ties between POIs by how widely OSM knows them ([8513479](https://github.com/open-geocode-rs/open-geocode/commit/8513479911fcfd19fdbe846c8ddc86c453e9855c))
* **search:** drop query words by label, not by count ([524ab03](https://github.com/open-geocode-rs/open-geocode/commit/524ab034a707b9a10301c2d00ccb5561221a99fd))
* **search:** filter search by country ([7216402](https://github.com/open-geocode-rs/open-geocode/commit/7216402a5e1e0ef80606bbcf69edf98397206bc3))
* **search:** fold Latin letters to ASCII in search keys ([ac51618](https://github.com/open-geocode-rs/open-geocode/commit/ac5161887aa1e3936bfc4e2eba0e2c0b04848528))
* **search:** index one text document per street group ([ae3e8d0](https://github.com/open-geocode-rs/open-geocode/commit/ae3e8d03cb5dd0187d31a72974b10696d6108a37))
* **search:** let a stricter later reading outweigh a lenient first one ([380dc5d](https://github.com/open-geocode-rs/open-geocode/commit/380dc5dd109b69080a600d2c2f3031de0b92d29a))
* **search:** plan queries from the address parser ([b0f9607](https://github.com/open-geocode-rs/open-geocode/commit/b0f9607f73aa91afb2caf42e4b53d9284be3b0aa))
* **search:** rank addresses above POIs when the query is an address ([fd9ba8a](https://github.com/open-geocode-rs/open-geocode/commit/fd9ba8a6a25b30241a4abab48efca6cd8c942525))
* **search:** rank ties by the caller's focus and filter by a box ([6e34fe1](https://github.com/open-geocode-rs/open-geocode/commit/6e34fe1e43e09263bd0f1073ecc5b364d087f863))
* **search:** rate each answer with a calibrated confidence ([8af4c8b](https://github.com/open-geocode-rs/open-geocode/commit/8af4c8bfc90a93dfd1647fc0c1b3a216a6438db0))
* **search:** search typos in tiers, exact first ([c1e1ed6](https://github.com/open-geocode-rs/open-geocode/commit/c1e1ed69622089b9f19c70854266cd43ad3b338e))
* **search:** weigh a postcode by how few addresses it covers in its country ([e4862bd](https://github.com/open-geocode-rs/open-geocode/commit/e4862bd7b400b8d8b5571252031380ec2191e0b0))
* **spatial:** index record blocks and lines in packed Hilbert R-trees ([f869667](https://github.com/open-geocode-rs/open-geocode/commit/f8696676cfd296b014196105361d24fcca2acf8d))
* stream builds to planet scale with a single-file Pack ([d6b8e85](https://github.com/open-geocode-rs/open-geocode/commit/d6b8e85061e7b94ba7b590c28590c874e5abab6c))


### Bug Fixes

* address review findings on the planet-scale build ([c661eb2](https://github.com/open-geocode-rs/open-geocode/commit/c661eb2791687d966ae7ccaa54f7b68aed97e00d))
* **address:** read slash house numbers from their street ([a4f4431](https://github.com/open-geocode-rs/open-geocode/commit/a4f44319300b9fdf4449d41799325558dca807b6))
* **batch:** pass the country filter to the searcher ([0863949](https://github.com/open-geocode-rs/open-geocode/commit/08639496a8cb4c5a65126e755036753058fd8921))
* **builder:** check the refined plan against the memory the machine has now ([55d3d6e](https://github.com/open-geocode-rs/open-geocode/commit/55d3d6ee385ec365d4db2525094a663407f5bb79))
* **builder:** cut pasted tags from values, rank no union of countries, drop a build's generation last ([2c42483](https://github.com/open-geocode-rs/open-geocode/commit/2c42483a8be2ac9c4963cd29000dc7ae390ae7c7))
* **builder:** keep boundary geometry inside the memory budget ([0e5d68b](https://github.com/open-geocode-rs/open-geocode/commit/0e5d68b933d6f6c1a18279df49aac9d3760b8768))
* **builder:** keep boundary shapes on disk so a planet build stays in its budget ([7c733ca](https://github.com/open-geocode-rs/open-geocode/commit/7c733caca540967fe8670b8fb062fcf249e01cf4))
* **build:** join the ways of a street within the area its label names ([684a179](https://github.com/open-geocode-rs/open-geocode/commit/684a1791baeb27ced84ac0aed333dc5252ec23db))
* **context:** rank a country with more admin levels than tiers ([e3e2c02](https://github.com/open-geocode-rs/open-geocode/commit/e3e2c02dccdccff649db0c53adde453910167e8f))
* **geocode:** accept a locality the record lies in at any tier or states ([66d373d](https://github.com/open-geocode-rs/open-geocode/commit/66d373dfcce3731fd3f68ee85fae9eb5d3f774fe))
* keep every build buffer within the memory budget ([598bd30](https://github.com/open-geocode-rs/open-geocode/commit/598bd30b47ab8fb93448fa613dede56b0c4b3beb))
* **labels:** show a bare unit as a unit, not a place ([428cf37](https://github.com/open-geocode-rs/open-geocode/commit/428cf373b603b39c12bc3b271a6c63672b9d5667))
* **pack:** publish validated generations without replacing live data ([c804e26](https://github.com/open-geocode-rs/open-geocode/commit/c804e26fe9cc7a0049e578ada8ed806168e7c49d))
* **pack:** read postcode address counts with as_chunks ([ead4a07](https://github.com/open-geocode-rs/open-geocode/commit/ead4a07da599389a3aa1df3c4b0e4f2c72492a9f))
* **parse:** check admin aliases against the names of areas only ([5575578](https://github.com/open-geocode-rs/open-geocode/commit/55755780e1fd706d302cd005dc910aeb699df574))
* **parse:** ground each closed-class label in a name of its own field ([4c1fa08](https://github.com/open-geocode-rs/open-geocode/commit/4c1fa08fc99ebb54d143b837b426f41bcbbbd093))
* **parse:** read a query as the countries its evidence points to ([58dc4e9](https://github.com/open-geocode-rs/open-geocode/commit/58dc4e964716654a22fca4ca9cf79ee5d5b06e26))
* **postcode:** place a postcode at the medoid of a bounded member sample, not the mean ([9290fd5](https://github.com/open-geocode-rs/open-geocode/commit/9290fd5d396fd3933beb7752d4196c50f86122aa))
* **postcode:** place a postcode at the medoid of a bounded member sample, not the mean ([aa4a593](https://github.com/open-geocode-rs/open-geocode/commit/aa4a593c5e19196226dfc5b12076d709047e6018)), closes [#3](https://github.com/open-geocode-rs/open-geocode/issues/3)
* **resources:** build on Windows under the denied lints ([6b9706d](https://github.com/open-geocode-rs/open-geocode/commit/6b9706dc66e260ec0461bc5864770dfe90b19933))
* **resources:** keep the rlimit cast for 32-bit targets ([db77683](https://github.com/open-geocode-rs/open-geocode/commit/db77683f59c3754e2779c30a816c873f7903fd2b))
* **runtime:** log the cause of every internal error under its request id ([816d331](https://github.com/open-geocode-rs/open-geocode/commit/816d331014d8c33e267052967fcc081c2614d893))
* **runtime:** log the cause of every internal error under its request id ([2dfdea8](https://github.com/open-geocode-rs/open-geocode/commit/2dfdea846da178ace2cee97bc95556310f8c9f83)), closes [#2](https://github.com/open-geocode-rs/open-geocode/issues/2)
* **search:** accept hits before the cut to the requested limit ([9200531](https://github.com/open-geocode-rs/open-geocode/commit/920053108eab4585094e549f0247ef33307d6d27))
* **search:** count an address's number among the words of its name ([84d3a54](https://github.com/open-geocode-rs/open-geocode/commit/84d3a54fd10fdf454452a2fe6ae0d02573d8662a))
* **search:** drop hits that contradict a stated postcode ([bbeb739](https://github.com/open-geocode-rs/open-geocode/commit/bbeb739c3e2e3f604c02323564d4afb7e67293bb))
* **search:** hold a known country's word to the country, and a town's node to its boundary ([0ea6075](https://github.com/open-geocode-rs/open-geocode/commit/0ea6075c858d20d7bc17b9738446445bed93f498))
* **search:** hold where the query says a record is in every reading ([606f883](https://github.com/open-geocode-rs/open-geocode/commit/606f88367f2177d150944062cdaca33beef3a867))
* **search:** judge a postcode by where its area lies, not by exact membership ([3808708](https://github.com/open-geocode-rs/open-geocode/commit/38087084d711eb547f908ebb40ae6af323e11ba1))
* **search:** keep an area from answering for every required street word ([2fdd31d](https://github.com/open-geocode-rs/open-geocode/commit/2fdd31d1e1e1c3995e746bb6f6fd4347bd04bd5a))
* **search:** keep the closest name and the larger place among equal scores ([8277798](https://github.com/open-geocode-rs/open-geocode/commit/8277798e7ae5e2431af8aed3e0cfe665f421dd23))
* **search:** let a given postcode win over one parsed from the address ([a7f9f77](https://github.com/open-geocode-rs/open-geocode/commit/a7f9f77b9bb202be06003ee7a71b29f549b075e7))
* **search:** let a named flat choose among one street's addresses, not outscore the street ([b40b012](https://github.com/open-geocode-rs/open-geocode/commit/b40b01256d000a2e1c355dd89714a4fc9118e241))
* **search:** let a number read as an unconfirmed postcode be the house ([b7c5cbc](https://github.com/open-geocode-rs/open-geocode/commit/b7c5cbcc2b6e8f0318f64d7a594c86a238dfb1bf))
* **search:** let a query of unplaced words drop one, and not for a street type ([6292398](https://github.com/open-geocode-rs/open-geocode/commit/62923983eff1b9e2c521bcff46270685c6a1f4c6))
* **search:** let a typo change a long word's second letter ([8ce75f4](https://github.com/open-geocode-rs/open-geocode/commit/8ce75f403e57da37b657b515b30b55c7b9761a17))
* **search:** put a stand-in street on its own centre line ([43ea64d](https://github.com/open-geocode-rs/open-geocode/commit/43ea64d67dfd83398f69fd65ae9f2932d989f41f))
* **search:** rank hits by distance from a named postcode before the cut ([264a87c](https://github.com/open-geocode-rs/open-geocode/commit/264a87cc091e1fd7e90c9a703a6ec1a54af9daf8))
* **search:** rank the street a query names above one with more words ([2a73703](https://github.com/open-geocode-rs/open-geocode/commit/2a737031d6427de76d0166ebf32c3815fc170bc4))
* **search:** read a flat named by a letter or written onto its number ([df5a334](https://github.com/open-geocode-rs/open-geocode/commit/df5a334b0dd96d82cf0c755b8271e2e09207c415))
* **search:** read a street-type abbreviation as every word it stands for ([b8939d7](https://github.com/open-geocode-rs/open-geocode/commit/b8939d7c70dfdd2bb4a957b3d4be4bd3bc9f0a29))


### Performance Improvements

* bound the string table by budget and parallelize anchor tags ([5f9ba50](https://github.com/open-geocode-rs/open-geocode/commit/5f9ba504ec8206f69845b74ca56376afe47ae4ba))
* **builder:** give ways their nodes from a node store, and plan from counted items ([725a620](https://github.com/open-geocode-rs/open-geocode/commit/725a6209c5e41ffd1e98d7e4ac6950f032c82e73))
* **builder:** keep the read buffer across seeks to nearby blobs ([3482b49](https://github.com/open-geocode-rs/open-geocode/commit/3482b49b65ff4189c7a69c53b1847ca43f969260))
* **builder:** rank each point in one pass, intern the country codes ([a5522c6](https://github.com/open-geocode-rs/open-geocode/commit/a5522c6570f705c827086e8839b70699245f4cda))
* **builder:** sort addresses by key and place, not by copies of the record ([a6b77b8](https://github.com/open-geocode-rs/open-geocode/commit/a6b77b8056d151508e53b326a57dd2822df40aa4))
* **build:** size the build to the host and train in seconds ([e399f49](https://github.com/open-geocode-rs/open-geocode/commit/e399f495475e5ed10069f433dacd3354d1a4ea2f))
* **parse:** decode more labellings only when the best are no address ([bb4c92a](https://github.com/open-geocode-rs/open-geocode/commit/bb4c92a7c993b176dc6a75c6d831007b3f831f68))
* **records:** build summaries without decoding geometry ([034b1e5](https://github.com/open-geocode-rs/open-geocode/commit/034b1e53af2f189bbae3d57baf2d371401199029))
* **spatial:** select nearest candidates before sorting ([88f97d5](https://github.com/open-geocode-rs/open-geocode/commit/88f97d5bbf0a1bee6b62acf53a67df6cd63bdb2a))
