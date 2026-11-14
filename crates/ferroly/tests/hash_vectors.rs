#![cfg(feature = "hash")]
//! Known-answer tests for the hashing, MAC and key-derivation primitives.
//!
//! Every expected value here comes from outside the implementation under
//! test: the FIPS 180-4 example messages, RFC 4231 (HMAC-SHA-2), RFC 2202
//! (HMAC-SHA-1), RFC 1321 (MD5), RFC 5869 (HKDF), RFC 7914 §11 (PBKDF2), the
//! CRC catalogue check value, and — for the boundary-length tables — digests
//! computed with an independent implementation (CPython's `hashlib`, `hmac`
//! and `zlib`, i.e. OpenSSL and zlib). Nothing is compared against itself.

use ferroly::hash::{
    base64, crc32, ct_eq, hex, hmac, hmac_sha1, hmac_sha256, hmac_sha384, hmac_sha512, kdf, md5,
    sha1, sha256, sha384, sha512, Crc32, Digest, HashAlgorithm, Hmac, Md5, Secret, Sha1, Sha256,
    Sha384, Sha512,
};

// ---- expected values (independent implementation / published) -------------

const SHA1_PATTERN: &[(usize, &str)] = &[
    (0, "da39a3ee5e6b4b0d3255bfef95601890afd80709"),
    (1, "5d1be7e9dda1ee8896be5b7e34a85ee16452a7b4"),
    (2, "7878ac025cfe0384191ff21ebb1627fd25f8a60c"),
    (54, "d55036939dd1f1b82217b436fd60b32b5d015ab1"),
    (55, "749bbefb28edc4638b28b2b9a9e03ab9a4032b90"),
    (56, "a5b6e9c29d201c774753ff8e7fb64931656f5e63"),
    (57, "eb0737bed5451790722b2df351829ce117e3d9dd"),
    (62, "0c7a70093fd6a9d8c0ee69571185f7a3334d5f9a"),
    (63, "d1a454409359fc372b4d22b3cea6488d6ba1be00"),
    (64, "39a0d8b645ad85f1f976731ed112ac9455e28b78"),
    (65, "d0c96e18890114a14716e9686528d2e3fdba8d9e"),
    (111, "b7b42d19ae6be209c36efe0c5dfe5bde4d306c43"),
    (112, "11e920cd4ed45c60c05a916e48a942f9e39c770b"),
    (119, "562ecf8a430f8e1056e3619bae33628e9a1d0a4e"),
    (120, "353f6d2bf0e91aa91b74a2e0b3f297510f7d825f"),
    (127, "bebc42d2d3d1e5fb8ad8895c2dcef2d68a6c279a"),
    (128, "0060f2a7e34b6e4d459f560197ef93243732a400"),
    (129, "3a16082d1bf09b604907ec6908b9893ca3e937c0"),
    (183, "870cbbfb74a4bb71369de543b8e0f3473f29a602"),
    (184, "3ab35d8a160f267b435f0a325ff461a64d3c7b7b"),
    (191, "90d881e31a36af55e527ceb7f011b051a6dd4ac2"),
    (192, "9d9112152625f518fae155f757471e6564167ac8"),
    (193, "d665859ce51ed06764f8be49466cd7bbfd9bdf06"),
    (1000, "414475341017ec91703435a6f290324818f983e9"),
];

const SHA256_PATTERN: &[(usize, &str)] = &[
    (
        0,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    ),
    (
        1,
        "ca358758f6d27e6cf45272937977a748fd88391db679ceda7dc7bf1f005ee879",
    ),
    (
        2,
        "140d811b81973993df99b8b1742b383ab83f6f52bf7af850812e7bba02ff11da",
    ),
    (
        54,
        "c802146d5788fb540fbf29d8ff485730ad10f4f13b78961c032e78691b582647",
    ),
    (
        55,
        "8aa994584139d128848eeebc4e815639ba5ab6e6e39574195a63ac4f14f7c43b",
    ),
    (
        56,
        "ad574708f75c044c9b85de64cb568ee7711ff4f36448c6242f053ba8f6cc2b63",
    ),
    (
        57,
        "5b46e502092be01b1100193e089fdda95638c12e19a1d24f308eb2c3d3ae849d",
    ),
    (
        62,
        "8a670c7c037c3947aa18d2a2a717c1814a210f51ea22138c5bc43d5e09f63db7",
    ),
    (
        63,
        "280ed3e8ff1df845b2e7dfe6ac6cee817bef20e783cc65abc41b818b4d2fe076",
    ),
    (
        64,
        "c6ab9724ade5b6a7a1edfffb12f3aa9181351355af8fd08c919952ad211339dd",
    ),
    (
        65,
        "788367c73c7ddf4c53f65e68cc0d943e6227ab55b0e78ba63ace822b1c6301c0",
    ),
    (
        111,
        "dd1413178fb627f9abbc041ffe39c44aa7aaa0e2e6d2ca5c4528ac7073a2da45",
    ),
    (
        112,
        "a65c92dac124062d0ab951a42773cb04fc98d1d4bf8897b176f8cff3509d379e",
    ),
    (
        119,
        "3d610547d68216dedf7435a4fb6260353911f6b3fd3f18805ddb8be285d726fe",
    ),
    (
        120,
        "1f80156a804cb7862ad113e8200e9d74499723e7c7854d5f48776d3148e09656",
    ),
    (
        127,
        "192409cd280e14b743642ad1343fbd3e82d9305de72c078117745a679210cc3d",
    ),
    (
        128,
        "cc548ca2dec1f6fe4f58b2e27aa9c7521607df1130d140b55a4dad0665302356",
    ),
    (
        129,
        "81e89a7b2911aaa7795f9e3d4910cb47d6cd2b00d83b8399481527261a1a7519",
    ),
    (
        183,
        "a0687018a48993fa01699bfd35af7ad7722645db72db8cb992ea29879f1f469a",
    ),
    (
        184,
        "3fc9f2e46a8dde9185fd546c4082719cd75ded2723835be16a07e61b1dea1595",
    ),
    (
        191,
        "2a30958d124d569d0a4832c608c772181557edbae684ff368be6592d3bf500c7",
    ),
    (
        192,
        "6e3a9b4ecba7af3a46e4f5c90fe02c99b5715144444b38049a42ac8313b30346",
    ),
    (
        193,
        "87746ac61c76c535aff44356de173c446cae1a259ea678f42af728c00b9aa287",
    ),
    (
        1000,
        "5097e7d587352f5097062ae679f37bda5802d9f875aba14c8cb4d1a188ada179",
    ),
];

const MD5_PATTERN: &[(usize, &str)] = &[
    (0, "d41d8cd98f00b204e9800998ecf8427e"),
    (1, "89e74e640b8c46257a29de0616794d5d"),
    (2, "e2cbbc39f325521a3fb6c4b370a40fe1"),
    (54, "3c600df998dc0e0610dc12c670346fce"),
    (55, "c9e512626618c9980ef21a96597af94c"),
    (56, "ecde7caa08e9f5657c863df107cac60a"),
    (57, "b17b1a018dd6a4d1edda8aca15f17846"),
    (62, "aaea5cebb2697f566c44424804472f5b"),
    (63, "2f0301069e1c40af7f6c8f843b1b13f2"),
    (64, "b6bf87c24b1bc334e2541387a92b981b"),
    (65, "f168246f08b6134d66bd2a10343fa9f1"),
    (111, "e145219ba825c566b3905b008ac4c378"),
    (112, "78e1458b5371b996a9d07fec4f8edd17"),
    (119, "d5dc3d8264de3aa24dee105910ee27fe"),
    (120, "bcc139b3848923904860d1eefd6e5923"),
    (127, "27670172396247fa5f68637f13f8933d"),
    (128, "3e85b70ffc8df5c735ecf2a8f14f1bee"),
    (129, "e45ecac9930fe93de302f98ba673ba36"),
    (183, "f9853a63992d4dc82fb4c2224bdc6855"),
    (184, "cb7726b9a4037c082461f27be10fb296"),
    (191, "f54fee284efd89eecc6136b0d931d172"),
    (192, "97ff8c2c469166fdfef87c190eccba0e"),
    (193, "183158f8a08682162a3462117eb65629"),
    (1000, "2b1e78d5765de9e10495a01412a1cf22"),
];

const SHA384_PATTERN: &[(usize, &str)] = &[
    (0, "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da274edebfe76f65fbd51ad2f14898b95b"),
    (1, "f12be95011fa37de81bf76bee4e73a992f431a7c69b60d2495c7383ae0545569842a0c019d89c208ae48f843562b56ff"),
    (110, "3b30c4743ed5b6f879e58fb99c738647fcbe47fc5d78f666121076a0a9e0ea8018a00f4a928fc22a3e39e9e96210f7a6"),
    (111, "5922d169e553267e1846ae2128a026407faf487dab98dfb383e751d3846f82f6edbe70b48b6202327516159d340c56a9"),
    (112, "b42aff1d6d298152801480cb2b5851c32afe5e5fbae583b0a9b52fe955440c3f276fd812c0f4439cf94c3f9e03068a21"),
    (113, "8cb479a9a1e2abe9e271a31f09534a46ea438842744703904558ca5849e7dc5ee7b62594e008962e4dac5da8b746bed6"),
    (119, "1a2bd69d2b680db1d83eb9b0c9ecf947c3fe4792965833e3de4785dabcddd726d997be6cc0abedb7b6d3455ccb936596"),
    (120, "7f33a13dc302aa532d01170692b78535d5c846eb9543de7a8d13a97054ddb7357abc5edf28eb6d0321634b81253df229"),
    (126, "fce4f6e1e6ddfe190fbae287bfae6d70f4e1ba15d647128cc3de16e7a5c28e1f2baa3943e278b68193d8e8235c498aeb"),
    (127, "e0cc0da38b3a32572851d199e9e485afe1e62f5c24ac1c7f0b6680a1ea9febbe49d02923617d02074dd2ad70b5671389"),
    (128, "95ab88109cbf117548a8ad35909ffefa175d06a9c42614b9d359548cff5d550b30affb36ff638cd32aa5f5409066c68a"),
    (129, "1d0599fa8c2d4e8d626778051da62b78a0ecc76cbfdc5687302048373d1f8a5e772f19aeaf7be25f68e0ccf4f0b7f8a8"),
    (239, "b0e8d8a1df31886ebb4707fbfce9ae5873f3dabcf6e7563f3352c1d837a5923ad7eedfe6b8c6d4a813095a30c352eb30"),
    (240, "62f145f55197394c113cdf35478b33900a179a2d2f7559f0584f914cc77dc49d631746abddd484e489ce7ab478a69d20"),
    (241, "2f58834dd15d818b8a9da6fe15e779e58f92782c51d3d5b80263775ba22d5f612eb29bd7276e2460ecdea0134e5992a4"),
    (255, "8d67a20eb619e87bd1083a1b616eb5406d77c8fc2b0894b7082e316d67e5b4d48fb45d3ca824fd204a8d47896a95368f"),
    (256, "1309811305efc73654ede73c40aaff0f19353c321689dcbb04138ab3fff3a6eabbc5a65af08da09fe44766d9e223826e"),
    (257, "7a990affd33d804967b571127e813efef6e54704ecb0c817c0afc703275d8c24c920b7b7d8bd2d57d9f84ea8e17f5d9f"),
    (383, "7dfc53be1b4153256606ddb85856b8eea9e6a0d1bc64b8564c5d828307d038a3cc93066b91238d42cfa3678e60b8014c"),
    (384, "88d7fa120025c4db8b97a24a33e8ca76be1f1b7bb3f30e92631c799547d4509d35e9017c552d8d23224bc878f47b73b3"),
    (1000, "4f33e6bdc22d2129245d832f3b149770b799aa9e63b4c79bd3073aca7e5afc8332c83cace1f5d81c22a256f09dcb3f98"),
];

const SHA512_PATTERN: &[(usize, &str)] = &[
    (0, "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"),
    (1, "365d11a1dfe610b60efa996136d37ab8afd2715b8c6bc2850dc5e6005b702bb9f59b0f306ecb2c43ee44c429967d45843524eb2f7c16aab9bde142ee268b51c6"),
    (110, "5d70edafefef2dec67b96c8df93eda04b3388b3eb653ca0b7e6d7d71a9b44d539dfa65023a2514f0a118fac1f6c0790bf9cf1169ee4ce00fa166f962979f240d"),
    (111, "da780d8338a8a920ceb6892cb4ecbb0cc0c66956269aadd5dd0f48790a00857bd975890f3b2955a317738cc7a770820c29f922ffbc22020f1909d594cc987d1b"),
    (112, "053182f7fa4e59f8636e415a77ed4fdc650f0a43834c9d35adf899599c3ab9c4153f02ff50bd01888060cd36a6fa12d9db242fc35164c80135613514186d5843"),
    (113, "766fea2f90094ecec1166ea4d6e9ddc6ae6fbe4a060e049300ecbe631a7d6547409b6ac39865409e6dd5dada112955fddf32698feb98806c1910f446ac5af7e3"),
    (119, "c6203c98db894a187e332570d8b0c317766be115afc7afa530e00ef42f2aa7492a7a4bba9b81762cf2a635c9bd22bde8fd85868d1214e48ae057cd7b69fdd7ff"),
    (120, "f138c42e1f58da8b1e7a14810a424cbe8b1baa4976b7853f4c9a856609fea684a6e9ee070abe81b88f3289b6711687eb751da453942e4eb6e5609212e8b08739"),
    (126, "400ec9ce81f97e31caadb4699f0f19e61116a60cfcf172898ddb5f8c56db8d50387898fa01b33e5fd6dd0d0d665ef9a347386ca48175c8189cd70ab5e0bd81f5"),
    (127, "a7a75593826fd37d4e60f6101eabb9f8ab1cf4d5319ebc805266d5da8deb5097de1a235fc5d9d3d73c50ac100ffc75089fb454674ab61232091bd19cbdc67396"),
    (128, "df007a08f3aaae47e0c92ef840ecd43645ae6098c819f2a4a66174ef1cd49e5c6dfccf0616895e570b7564af641de5863dff9f89c752913d30cf0ecf678e1635"),
    (129, "a10ea769b79831bca33e9efb29fc501ded6e9c88885d079acb76eecaa8ac58d68bdc7e00103053e1519352c0d064c87accc3d553a6039decc2c4f1e7667eef79"),
    (239, "38980139aa28f1ccec548e21fd73d927488c4aa749a4b304fe0c5f7797fb80a03bc0557584ef42f47e0d3ff4eee0011084ac8421cf08dd2cea6a132bcd075c90"),
    (240, "eb5690f4b1753298de6626f031576ca2b93cec042d597a7a1f9e47534af610f409acb608d88fdaeade8b36fd09a6cd81f2ff8ec10962e79cd791eebc26d569f5"),
    (241, "251de04cf5e385bd219874e1d5b9bd8922411704029606ac7b99915032fa4bc7fb8435b84995f14a8707ff154c9c3d13332a1e4c2233ccbfa3494b91558bfd5f"),
    (255, "ac1ecd53106f3dfba66b7b3fcac37652aad2aad2a291c354eaca03928e74853d53c0e8c7f2fa05025efdb262fd4b51103747d3dafd53e1feefc590bf2617b30a"),
    (256, "7629794792236644c8c5792257d9a64320b887aae34806f9bc3713c494ea7f17602e0a9a9009b1e69b31d189d89a4f599dd30891fa04a1668ff31e30d0719df4"),
    (257, "fa398228112256bf360b7f6693efec0aecd70147173ab48853f8b99f5e30861868b9ce341bde0e47baa31ecb3a9e773d6fa8996b112e108b31e6522bc8de8ab4"),
    (383, "05596c27db550e3354382152d22288cb47ae02d47432296a5591942128963da4a0279355924f9117ce62164839d1fe1e6e3670779b46a3a20260b50378c6966b"),
    (384, "9cec19cd0f099553bf15a68861a289df3d58c278315a17442b8c16338deb8b48fe07a09fddde6253a6c80b9bdde319f4c2f471dcd32ede81e90116464e62c4f8"),
    (1000, "b41d42e106eca6bf57123566b7ed1550c37d33af23afbfa8e302dfd44c988b3003a26b4140ef42f535e66ea424e7c4a31616307269e6d520a6508f726da23d0a"),
];

const SHA1_FIPS: [&str; 5] = [
    "da39a3ee5e6b4b0d3255bfef95601890afd80709",
    "a9993e364706816aba3e25717850c26c9cd0d89d",
    "84983e441c3bd26ebaae4aa1f95129e5e54670f1",
    "a49b2446a02c645bf419f995b67091253a04a259",
    "34aa973cd4c4daa4f61eeb2bdbad27316534016f",
];

const SHA256_FIPS: [&str; 5] = [
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
    "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1",
    "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
];

const SHA384_FIPS: [&str; 5] = [
    "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da274edebfe76f65fbd51ad2f14898b95b",
    "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7",
    "3391fdddfc8dc7393707a65b1b4709397cf8b1d162af05abfe8f450de5f36bc6b0455a8520bc4e6f5fe95b1fe3c8452b",
    "09330c33f71147e83d192fc782cd1b4753111b173b3b05d22fa08086e3b0f712fcc7c71a557e2db966c3e9fa91746039",
    "9d0e1809716474cb086e834e310a4a1ced149e9c00f248527972cec5704c2a5b07b8b3dc38ecc4ebae97ddd87f3d8985",
];

const SHA512_FIPS: [&str; 5] = [
    "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e",
    "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
    "204a8fc6dda82f0a0ced7beb8e08a41657c16ef468b228a8279be331a703c33596fd15c13b1b07f9aa1d3bea57789ca031ad85c7a71dd70354ec631238ca3445",
    "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909",
    "e718483d0ce769644e2e42c7bc15b4638e1f98b13b2044285632a803afa973ebde0ff244877ea60a4cb0432ce577c31beb009c5c2c49aa2e4eadb217ad8cc09b",
];

const MD5_FIPS: [&str; 5] = [
    "d41d8cd98f00b204e9800998ecf8427e",
    "900150983cd24fb0d6963f7d28e17f72",
    "8215ef0796a20bcaaae116d3876c664a",
    "03dd8807a93175fb062dfb55dc7d359c",
    "7707d6ae4e027c70eea2a935c2296f21",
];

/// RFC 4231 §4.2–4.8 expected tags (case 5 is the full, untruncated tag).
const HMAC_SHA256_RFC4231: [&str; 7] = [
    "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
    "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
    "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe",
    "82558a389a443c0ea4cc819899f2083a85f0faa3e578f8077a2e3ff46729665b",
    "a3b6167473100ee06e0c796c2955552bfa6f7c0a6a8aef8b93f860aab0cd20c5",
    "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
    "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2",
];

/// RFC 4231 §4.2–4.8 expected tags (case 5 is the full, untruncated tag).
const HMAC_SHA384_RFC4231: [&str; 7] = [
    "afd03944d84895626b0825f4ab46907f15f9dadbe4101ec682aa034c7cebc59cfaea9ea9076ede7f4af152e8b2fa9cb6",
    "af45d2e376484031617f78d2b58a6b1b9c7ef464f5a01b47e42ec3736322445e8e2240ca5e69e2c78b3239ecfab21649",
    "88062608d3e6ad8a0aa2ace014c8a86f0aa635d947ac9febe83ef4e55966144b2a5ab39dc13814b94e3ab6e101a34f27",
    "3e8a69b7783c25851933ab6290af6ca77a9981480850009cc5577c6e1f573b4e6801dd23c4a7d679ccf8a386c674cffb",
    "3abf34c3503b2a23a46efc619baef897f4c8e42c934ce55ccbae9740fcbc1af4ca62269e2a37cd88ba926341efe4aeea",
    "4ece084485813e9088d2c63a041bc5b44f9ef1012a2b588f3cd11f05033ac4c60c2ef6ab4030fe8296248df163f44952",
    "6617178e941f020d351e2f254e8fd32c602420feb0b8fb9adccebb82461e99c5a678cc31e799176d3860e6110c46523e",
];

/// RFC 4231 §4.2–4.8 expected tags (case 5 is the full, untruncated tag).
const HMAC_SHA512_RFC4231: [&str; 7] = [
    "87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cdedaa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854",
    "164b7a7bfcf819e2e395fbe73b56e0a387bd64222e831fd610270cd7ea2505549758bf75c05a994a6d034f65f8f0e6fdcaeab1a34d4a6b4b636e070a38bce737",
    "fa73b0089d56a284efb0f0756c890be9b1b5dbdd8ee81a3655f83e33b2279d39bf3e848279a722c806b485a47e67c807b946a337bee8942674278859e13292fb",
    "b0ba465637458c6990e5a8c5f61d4af7e576d97ff94b872de76f8050361ee3dba91ca5c11aa25eb4d679275cc5788063a5f19741120c4f2de2adebeb10a298dd",
    "415fad6271580a531d4179bc891d87a650188707922a4fbb36663a1eb16da008711c5b50ddd0fc235084eb9d3364a1454fb2ef67cd1d29fe6773068ea266e96b",
    "80b24263c7c1a3ebb71493c1dd7be8b49b46d1f41b4aeec1121b013783f8f3526b56d037e05f2598bd0fd2215d6a1e5295e64f73f63f0aec8b915a985d786598",
    "e37b6a775dc87dbaa4dfa9f96e5e3ffddebd71f8867289865df5a32d20cdc944b6022cac3c4982b10d5eeb55c3e4de15134676fb6de0446065c97440fa8c6a58",
];

/// RFC 1321 appendix A.5 test suite.
const MD5_RFC1321: [(&str, &str); 7] = [
    ("", "d41d8cd98f00b204e9800998ecf8427e"),
    ("a", "0cc175b9c0f1b6a831c399e269772661"),
    ("abc", "900150983cd24fb0d6963f7d28e17f72"),
    ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
    (
        "abcdefghijklmnopqrstuvwxyz",
        "c3fcd3d76192e4007dfb496cca67e13b",
    ),
    (
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
        "d174ab98d277d9f5a5611c2c9f419d9f",
    ),
    (
        "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
        "57edf4a22be3c955ac49da2e2107b67a",
    ),
];

const CRC32_PATTERN_1000: u32 = 0x8902161e;
const CRC32_A: u32 = 0xe8b7be43;
const CRC32_ABC: u32 = 0x352441c2;

/// RFC 5869 appendix A.1–A.3: (salt, ikm, info, prk, okm), all hex.
const HKDF_SHA256_RFC5869: [(&str, &str, &str, &str, &str); 3] = [
    ("000102030405060708090a0b0c", "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b", "f0f1f2f3f4f5f6f7f8f9", "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5", "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"),
    ("606162636465666768696a6b6c6d6e6f707172737475767778797a7b7c7d7e7f808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9fa0a1a2a3a4a5a6a7a8a9aaabacadaeaf", "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f404142434445464748494a4b4c4d4e4f", "b0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedfe0e1e2e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeff", "06a6b88c5853361a06104c9ceb35b45cef760014904671014a193f40c15fc244", "b11e398dc80327a1c8e7f78c596a49344f012eda2d4efad8a050cc4c19afa97c59045a99cac7827271cb41c65e590e09da3275600c2f09b8367793a9aca3db71cc30c58179ec3e87c14c01d5c1f3434f1d87"),
    ("", "0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b", "", "19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04", "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8"),
];

const HKDF_SHA512_REFERENCE: &str = "4013bb31df22dee74d5f90a44cf603c7a68d9e462ae60f1b4ca501e1ca1e6015d8c1ad2b73a20ecdfae77b1e68e8efda53387f3d52997a4af2783891e0634712d173b2a24fd42417c286d621f900e917b631d43e38f66c9a58a42f4566a1380c2f25f94a";

/// PBKDF2-HMAC-SHA256: (password, salt, iterations, derived key hex).
/// The first two rows are RFC 7914 §11; the rest are the widely published
/// RFC 6070-style inputs evaluated with SHA-256.
const PBKDF2_SHA256: [(&[u8], &[u8], u32, &str); 8] = [
    (b"passwd", b"salt", 1, "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc49ca9cccf179b645991664b39d77ef317c71b845b1e30bd509112041d3a19783"),
    (b"Password", b"NaCl", 80000, "4ddcd8f60b98be21830cee5ef22701f9641a4418d04c0414aeff08876b34ab56a1d425a1225833549adb841b51c9b3176a272bdebba1d078478f62b397f33c8d"),
    (b"password", b"salt", 1, "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"),
    (b"password", b"salt", 2, "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"),
    (b"password", b"salt", 4096, "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"),
    (b"passwordPASSWORDpassword", b"saltSALTsaltSALTsaltSALTsaltSALTsalt", 4096, "348c89dbcbd32b2f32d814b8116e84cf2b17347ebc1800181c4e2a1fb8dd53e1c635518c7dac47e9"),
    (b"pass\x00word", b"sa\x00lt", 4096, "89b69d0516f829893c696226650a8687"),
    (b"", b"salt", 3, "5ddf839afa2d5fb4be56e1a0f48917617559bef61ec122bfca1c7f75ac8f401d06"),
];

const PBKDF2_SHA512_REFERENCE: &str = "afe6c5530785b6cc6b1c6453384731bd5ee432ee549fd42fb6695779ad8a1c5bf59de69c48f774efc4007d5298f9033c0241d5ab69305e7b64eceeb8d834cfec6afdec3c1c23";

// ---- helpers -------------------------------------------------------------

/// The deterministic test message used by the `*_PATTERN` tables.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 + 7) as u8).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    hex::decode(text).expect("valid hex in test vector")
}

/// One-shot digest through the generic [`HashAlgorithm`] trait.
fn digest_hex<H: HashAlgorithm>(data: &[u8]) -> String {
    let mut h = H::new();
    h.update(data);
    hex::encode(h.finalize().as_ref())
}

/// Digest of `data` fed as two updates split at `at`.
fn split_hex<H: HashAlgorithm>(data: &[u8], at: usize) -> String {
    let mut h = H::new();
    h.update(&data[..at]);
    h.update(&data[at..]);
    hex::encode(h.finalize().as_ref())
}

const MSG_448: &[u8] = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
const MSG_896: &[u8] = b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmnhijklmn\
oijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu";

fn fips_messages() -> [Vec<u8>; 5] {
    [
        Vec::new(),
        b"abc".to_vec(),
        MSG_448.to_vec(),
        MSG_896.to_vec(),
        vec![b'a'; 1_000_000],
    ]
}

fn check_fips<H: HashAlgorithm>(expected: &[&str; 5]) {
    for (msg, want) in fips_messages().iter().zip(expected) {
        assert_eq!(
            &digest_hex::<H>(msg),
            want,
            "message of {} bytes",
            msg.len()
        );
    }
}

fn check_pattern<H: HashAlgorithm>(table: &[(usize, &str)]) {
    for &(n, want) in table {
        let data = pattern(n);
        assert_eq!(digest_hex::<H>(&data), want, "one-shot, n={n}");
        // Every two-way split must agree with the independent digest.
        for at in 0..=n.min(300) {
            assert_eq!(split_hex::<H>(&data, at), want, "n={n} split at {at}");
        }
        // Byte-at-a-time and odd-sized chunks.
        for step in [1usize, 3, 7, 61, 67, 127, 129] {
            let mut h = H::new();
            for chunk in data.chunks(step) {
                h.update(chunk);
            }
            assert_eq!(
                hex::encode(h.finalize().as_ref()),
                want,
                "n={n} step={step}"
            );
        }
    }
}

// ---- FIPS 180-4 example messages ------------------------------------------

#[test]
fn sha256_fips_180_4_vectors() {
    check_fips::<Sha256>(&SHA256_FIPS);
    // The published values, spelled out for the two shortest messages.
    assert_eq!(
        sha256(b"").to_hex(),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256(b"abc").to_hex(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256(MSG_448).to_hex(),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

#[test]
fn sha512_fips_180_4_vectors() {
    check_fips::<Sha512>(&SHA512_FIPS);
    assert_eq!(
        sha512(b"").to_hex(),
        "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
         47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
    );
    assert_eq!(
        sha512(b"abc").to_hex(),
        "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
         2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
    );
    assert_eq!(
        sha512(MSG_896).to_hex(),
        "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018\
         501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909"
    );
    assert_eq!(
        sha512(&vec![b'a'; 1_000_000]).to_hex(),
        "e718483d0ce769644e2e42c7bc15b4638e1f98b13b2044285632a803afa973eb\
         de0ff244877ea60a4cb0432ce577c31beb009c5c2c49aa2e4eadb217ad8cc09b"
    );
}

#[test]
fn sha384_fips_180_4_vectors() {
    check_fips::<Sha384>(&SHA384_FIPS);
    assert_eq!(
        sha384(b"").to_hex(),
        "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da\
         274edebfe76f65fbd51ad2f14898b95b"
    );
    assert_eq!(
        sha384(b"abc").to_hex(),
        "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed\
         8086072ba1e7cc2358baeca134c825a7"
    );
    assert_eq!(
        sha384(MSG_896).to_hex(),
        "09330c33f71147e83d192fc782cd1b4753111b173b3b05d22fa08086e3b0f712\
         fcc7c71a557e2db966c3e9fa91746039"
    );
    assert_eq!(
        sha384(&vec![b'a'; 1_000_000]).to_hex(),
        "9d0e1809716474cb086e834e310a4a1ced149e9c00f248527972cec5704c2a5b\
         07b8b3dc38ecc4ebae97ddd87f3d8985"
    );
}

#[test]
fn sha1_fips_180_vectors() {
    check_fips::<Sha1>(&SHA1_FIPS);
    assert_eq!(
        sha1(b"abc").to_hex(),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
    assert_eq!(
        sha1(MSG_448).to_hex(),
        "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
    );
    assert_eq!(
        sha1(&vec![b'a'; 1_000_000]).to_hex(),
        "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
    );
}

// ---- boundary lengths and split updates -----------------------------------

#[test]
fn sha256_boundary_lengths_and_splits() {
    check_pattern::<Sha256>(SHA256_PATTERN);
}

#[test]
fn sha1_boundary_lengths_and_splits() {
    check_pattern::<Sha1>(SHA1_PATTERN);
}

#[test]
fn md5_boundary_lengths_and_splits() {
    check_pattern::<Md5>(MD5_PATTERN);
}

#[test]
fn sha512_boundary_lengths_and_splits() {
    check_pattern::<Sha512>(SHA512_PATTERN);
}

#[test]
fn sha384_boundary_lengths_and_splits() {
    check_pattern::<Sha384>(SHA384_PATTERN);
}

#[test]
fn sha512_split_update_equivalence_at_block_boundaries() {
    // The lengths where the 128-byte block and its 16-byte length field
    // interact: one byte short of forcing a second padding block (111), exactly
    // forcing it (112), one short of a full block (127) and a full block (128).
    for n in [111usize, 112, 127, 128] {
        let data = pattern(n);
        let want512 = sha512(&data);
        let want384 = sha384(&data);
        let table512 = SHA512_PATTERN.iter().find(|(len, _)| *len == n).unwrap().1;
        let table384 = SHA384_PATTERN.iter().find(|(len, _)| *len == n).unwrap().1;
        assert_eq!(want512.to_hex(), table512);
        assert_eq!(want384.to_hex(), table384);
        for a in 0..=n {
            for b in a..=n {
                let mut h = Sha512::new();
                h.update(&data[..a]);
                h.update(&data[a..b]);
                h.update(&data[b..]);
                assert_eq!(h.finalize(), want512, "sha512 n={n} a={a} b={b}");
            }
            let mut h = Sha384::new();
            h.update(&data[..a]);
            h.update(&[]);
            h.update(&data[a..]);
            assert_eq!(h.finalize(), want384, "sha384 n={n} a={a}");
        }
    }
}

#[test]
fn cloned_hasher_state_forks_independently() {
    let mut base = Sha512::new();
    base.update(b"common prefix ");
    let mut left = base.clone();
    let mut right = base;
    left.update(b"left");
    right.update(b"right");
    assert_eq!(left.finalize(), sha512(b"common prefix left"));
    assert_eq!(right.finalize(), sha512(b"common prefix right"));
}

// ---- HMAC -----------------------------------------------------------------

/// RFC 4231 §4.2–4.8 keys and messages.
fn rfc4231_cases() -> [(Vec<u8>, Vec<u8>); 7] {
    [
        (vec![0x0b; 20], b"Hi There".to_vec()),
        (b"Jefe".to_vec(), b"what do ya want for nothing?".to_vec()),
        (vec![0xaa; 20], vec![0xdd; 50]),
        ((1u8..=25).collect(), vec![0xcd; 50]),
        (vec![0x0c; 20], b"Test With Truncation".to_vec()),
        (
            vec![0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First".to_vec(),
        ),
        (
            vec![0xaa; 131],
            b"This is a test using a larger than block-size key and a larger \
than block-size data. The key needs to be hashed before being \
used by the HMAC algorithm."
                .to_vec(),
        ),
    ]
}

fn check_rfc4231<H: HashAlgorithm>(expected: &[&str; 7]) {
    for (i, ((key, msg), want)) in rfc4231_cases().iter().zip(expected).enumerate() {
        let case = i + 1;
        assert_eq!(
            &hex::encode(hmac::<H>(key, msg).as_ref()),
            want,
            "RFC 4231 test case {case}"
        );
        // Streaming in three pieces gives the same tag.
        let mut mac = Hmac::<H>::new(key);
        let third = msg.len() / 3;
        mac.update(&msg[..third]);
        mac.update(&msg[third..2 * third]);
        mac.update(&msg[2 * third..]);
        assert_eq!(
            &hex::encode(mac.finalize().as_ref()),
            want,
            "case {case} streamed"
        );
        // verify(): accepts the tag, rejects a flipped bit and a truncation.
        let tag = unhex(want);
        let keyed = Hmac::<H>::new(key);
        let mut ok = keyed.clone();
        ok.update(msg);
        assert!(ok.verify(&tag));
        let mut flipped = tag.clone();
        flipped[0] ^= 1;
        let mut bad = keyed.clone();
        bad.update(msg);
        assert!(!bad.verify(&flipped));
        let mut short = keyed;
        short.update(msg);
        assert!(!short.verify(&tag[..tag.len() - 1]));
    }
}

#[test]
fn hmac_sha256_rfc4231_all_cases() {
    check_rfc4231::<Sha256>(&HMAC_SHA256_RFC4231);
    // Spelled-out published values for the long-key cases (6 and 7), where
    // the 131-byte key exceeds the 64-byte block and is hashed first.
    let cases = rfc4231_cases();
    assert_eq!(
        hmac_sha256(&cases[5].0, &cases[5].1).to_hex(),
        "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
    );
    assert_eq!(
        hmac_sha256(&cases[6].0, &cases[6].1).to_hex(),
        "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
    );
    // Case 5 is published truncated to 128 bits.
    assert_eq!(
        &hmac_sha256(&cases[4].0, &cases[4].1).to_hex()[..32],
        "a3b6167473100ee06e0c796c2955552b"
    );
}

#[test]
fn hmac_sha384_rfc4231_all_cases() {
    check_rfc4231::<Sha384>(&HMAC_SHA384_RFC4231);
    let cases = rfc4231_cases();
    assert_eq!(
        hmac_sha384(&cases[0].0, &cases[0].1).to_hex(),
        "afd03944d84895626b0825f4ab46907f15f9dadbe4101ec682aa034c7cebc59c\
         faea9ea9076ede7f4af152e8b2fa9cb6"
    );
    assert_eq!(
        hmac_sha384(&cases[1].0, &cases[1].1).to_hex(),
        "af45d2e376484031617f78d2b58a6b1b9c7ef464f5a01b47e42ec3736322445e\
         8e2240ca5e69e2c78b3239ecfab21649"
    );
}

#[test]
fn hmac_sha512_rfc4231_all_cases() {
    check_rfc4231::<Sha512>(&HMAC_SHA512_RFC4231);
    let cases = rfc4231_cases();
    assert_eq!(
        hmac_sha512(&cases[0].0, &cases[0].1).to_hex(),
        "87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cde\
         daa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854"
    );
    assert_eq!(
        hmac_sha512(&cases[1].0, &cases[1].1).to_hex(),
        "164b7a7bfcf819e2e395fbe73b56e0a387bd64222e831fd610270cd7ea250554\
         9758bf75c05a994a6d034f65f8f0e6fdcaeab1a34d4a6b4b636e070a38bce737"
    );
}

#[test]
fn hmac_sha1_rfc2202_vectors() {
    assert_eq!(
        hmac_sha1(&[0x0b; 20], b"Hi There").to_hex(),
        "b617318655057264e28bc0b6fb378c8ef146be00"
    );
    assert_eq!(
        hmac_sha1(b"Jefe", b"what do ya want for nothing?").to_hex(),
        "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
    );
    assert_eq!(
        hmac_sha1(&[0xaa; 20], &[0xdd; 50]).to_hex(),
        "125d7342b9ac11cd91a39af48aa17b4f63f175d3"
    );
    // 80-byte keys exceed SHA-1's 64-byte block.
    assert_eq!(
        hmac_sha1(
            &[0xaa; 80],
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        )
        .to_hex(),
        "aa4ae5e15272d00e95705637ce8a3b55ed402112"
    );
    assert_eq!(
        hmac_sha1(
            &[0xaa; 80],
            b"Test Using Larger Than Block-Size Key and Larger Than One Block-Size Data"
        )
        .to_hex(),
        "e8e99d0f45237d786d6bbaa7965c7808bbff1a91"
    );
}

#[test]
fn hmac_key_exactly_one_block_is_not_hashed() {
    // A key of exactly the block length is used as-is; one byte longer is
    // hashed first. Both must differ from each other and be stable under
    // streaming.
    for (block, key_byte) in [(64usize, 0x11u8), (128, 0x22)] {
        let exact = vec![key_byte; block];
        let longer = vec![key_byte; block + 1];
        if block == 64 {
            let a = hmac_sha256(&exact, b"m");
            let b = hmac_sha256(&longer, b"m");
            assert_ne!(a, b);
            // A long key is equivalent to its own digest used as the key.
            assert_eq!(b, hmac_sha256(sha256(&longer).as_bytes(), b"m"));
        } else {
            let a = hmac_sha512(&exact, b"m");
            let b = hmac_sha512(&longer, b"m");
            assert_ne!(a, b);
            assert_eq!(b, hmac_sha512(sha512(&longer).as_bytes(), b"m"));
            let c = hmac_sha384(&longer, b"m");
            assert_eq!(c, hmac_sha384(sha384(&longer).as_bytes(), b"m"));
        }
    }
}

// ---- MD5 and CRC-32 -------------------------------------------------------

#[test]
fn md5_rfc1321_test_suite() {
    for (msg, want) in MD5_RFC1321 {
        assert_eq!(md5(msg.as_bytes()).to_hex(), want, "{msg:?}");
    }
    check_fips::<Md5>(&MD5_FIPS);
    assert_eq!(md5(b"").to_hex(), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(
        md5(b"The quick brown fox jumps over the lazy dog").to_hex(),
        "9e107d9d372bb6826bd81d3542a419d6"
    );
}

#[test]
fn crc32_ieee_check_values() {
    // The catalogued check value of CRC-32/ISO-HDLC.
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    assert_eq!(crc32(b""), 0);
    assert_eq!(crc32(b"a"), CRC32_A);
    assert_eq!(crc32(b"abc"), CRC32_ABC);
    assert_eq!(
        crc32(b"The quick brown fox jumps over the lazy dog"),
        0x414f_a339
    );
    let data = pattern(1000);
    assert_eq!(crc32(&data), CRC32_PATTERN_1000);
    for step in [1usize, 2, 7, 64, 999] {
        let mut crc = Crc32::new();
        for chunk in data.chunks(step) {
            crc.update(chunk);
        }
        assert_eq!(crc.finalize(), CRC32_PATTERN_1000, "step={step}");
    }
    // `finalize` does not consume: a running checksum can be read mid-stream.
    let mut crc = Crc32::new();
    crc.update(b"1234");
    let _ = crc.finalize();
    crc.update(b"56789");
    assert_eq!(crc.finalize(), 0xcbf4_3926);
}

// ---- key derivation -------------------------------------------------------

#[test]
fn hkdf_sha256_rfc5869_test_cases() {
    for (i, (salt, ikm, info, prk, okm)) in HKDF_SHA256_RFC5869.iter().enumerate() {
        let case = i + 1;
        let (salt, ikm, info) = (unhex(salt), unhex(ikm), unhex(info));
        let want_okm = unhex(okm);

        let got_prk = kdf::hkdf_extract::<Sha256>(&salt, &ikm);
        assert_eq!(&got_prk.to_hex(), prk, "PRK, case {case}");

        let mut out = vec![0u8; want_okm.len()];
        kdf::hkdf_expand::<Sha256>(got_prk.as_bytes(), &info, &mut out).unwrap();
        assert_eq!(out, want_okm, "OKM via expand, case {case}");

        let mut out = vec![0u8; want_okm.len()];
        kdf::hkdf_sha256(&salt, &ikm, &info, &mut out).unwrap();
        assert_eq!(out, want_okm, "OKM one-shot, case {case}");

        // A shorter request is a prefix of a longer one.
        let mut short = vec![0u8; 17];
        kdf::hkdf_sha256(&salt, &ikm, &info, &mut short).unwrap();
        assert_eq!(short, want_okm[..17]);
    }
    // Published values for case 1, spelled out.
    assert_eq!(
        HKDF_SHA256_RFC5869[0].4,
        "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf\
         34007208d5b887185865"
    );
}

#[test]
fn hkdf_limits_and_other_hashes() {
    // Exactly 255 blocks is allowed; one byte more is not.
    let mut max = vec![0u8; 255 * 32];
    assert!(kdf::hkdf_sha256(b"s", b"ikm", b"", &mut max).is_ok());
    let mut too_long = vec![0u8; 255 * 32 + 1];
    assert_eq!(
        kdf::hkdf_sha256(b"s", b"ikm", b"", &mut too_long),
        Err(kdf::KdfError::OutputTooLong)
    );
    assert!(kdf::hkdf_sha256(b"s", b"ikm", b"", &mut []).is_ok());

    let mut out = vec![0u8; 100];
    kdf::hkdf::<Sha512>(b"salt", b"input key material", b"ctx", &mut out).unwrap();
    assert_eq!(hex::encode(&out), HKDF_SHA512_REFERENCE);

    // Different `info` strings give unrelated keys.
    let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
    kdf::hkdf_sha256(b"salt", b"ikm", b"a", &mut a).unwrap();
    kdf::hkdf_sha256(b"salt", b"ikm", b"b", &mut b).unwrap();
    assert_ne!(a, b);
}

#[test]
fn pbkdf2_hmac_sha256_published_vectors() {
    for (password, salt, iterations, want) in PBKDF2_SHA256 {
        let want = unhex(want);
        let mut out = vec![0u8; want.len()];
        kdf::pbkdf2_hmac_sha256(password, salt, iterations, &mut out).unwrap();
        assert_eq!(out, want, "c={iterations}, dkLen={}", want.len());
    }
    // RFC 7914 §11, first vector, spelled out.
    let mut out = [0u8; 64];
    kdf::pbkdf2_hmac_sha256(b"passwd", b"salt", 1, &mut out).unwrap();
    assert_eq!(
        hex::encode(&out),
        "55ac046e56e3089fec1691c22544b605f94185216dde0465e68b9d57c20dacbc\
         49ca9cccf179b645991664b39d77ef317c71b845b1e30bd509112041d3a19783"
    );
}

#[test]
fn pbkdf2_parameters_and_other_hashes() {
    let mut out = [0u8; 16];
    assert_eq!(
        kdf::pbkdf2_hmac_sha256(b"p", b"s", 0, &mut out),
        Err(kdf::KdfError::ZeroIterations)
    );
    assert!(kdf::pbkdf2_hmac_sha256(b"p", b"s", 1, &mut []).is_ok());

    let mut out = vec![0u8; 70];
    kdf::pbkdf2_hmac::<Sha512>(b"password", b"salt", 1000, &mut out).unwrap();
    assert_eq!(hex::encode(&out), PBKDF2_SHA512_REFERENCE);
}

// ---- comparison, digests, encodings ---------------------------------------

#[test]
fn ct_eq_agrees_with_plain_equality() {
    assert!(ct_eq(b"", b""));
    assert!(ct_eq(b"a", b"a"));
    assert!(!ct_eq(b"a", b"b"));
    assert!(!ct_eq(b"a", b""));
    assert!(!ct_eq(b"abc", b"abcd"));
    let base = pattern(64);
    for i in 0..base.len() {
        for bit in 0..8 {
            let mut other = base.clone();
            other[i] ^= 1 << bit;
            assert!(!ct_eq(&base, &other), "byte {i} bit {bit}");
        }
    }
    assert!(ct_eq(&base, &base.clone()));
}

#[test]
fn digest_hex_round_trip_and_rejection() {
    let d = sha256(b"abc");
    assert_eq!(Digest::<32>::from_hex(&d.to_hex()).unwrap(), d);
    assert_eq!(
        Digest::<32>::from_hex(&d.to_hex().to_uppercase()).unwrap(),
        d
    );
    assert!(d.ct_eq(&Digest::from_hex(&d.to_hex()).unwrap()));
    // Wrong length (a SHA-1 hex into a 32-byte digest), odd length, bad digit.
    assert_eq!(
        Digest::<32>::from_hex(&sha1(b"abc").to_hex()),
        Err(hex::DecodeError::InvalidLength)
    );
    assert_eq!(
        Digest::<32>::from_hex("abc"),
        Err(hex::DecodeError::OddLength)
    );
    let mut bad = d.to_hex();
    bad.replace_range(10..11, "g");
    assert_eq!(
        Digest::<32>::from_hex(&bad),
        Err(hex::DecodeError::InvalidCharacter { index: 10 })
    );
}

#[test]
fn base64_and_hex_are_reachable_from_the_public_api() {
    let tag = hmac_sha256(b"key", b"msg");
    let b64 = base64::encode(tag.as_bytes());
    assert_eq!(base64::decode(&b64).unwrap(), tag.as_bytes());
    let url = base64::encode_url_no_pad(tag.as_bytes());
    assert!(!url.contains('=') && !url.contains('+') && !url.contains('/'));
    assert_eq!(base64::decode_url_no_pad(&url).unwrap(), tag.as_bytes());
    assert_eq!(hex::decode(tag.to_hex()).unwrap(), tag.as_bytes());
}

#[test]
fn secret_is_redacted_and_comparable() {
    let secret = Secret::from("correct horse battery staple");
    assert_eq!(format!("{secret:?}"), "Secret([REDACTED])");
    assert_eq!(secret.to_string(), "[REDACTED]");
    assert_eq!(secret.expose_str(), Some("correct horse battery staple"));
    assert!(secret.ct_eq(b"correct horse battery staple"));
    // A Secret works directly as an HMAC key.
    assert_eq!(
        hmac_sha256(secret.expose(), b"m"),
        hmac_sha256(b"correct horse battery staple", b"m")
    );
}
