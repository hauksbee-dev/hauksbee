# Dynamic-parameter audit of the curated BJT, diode and MOSFET cards

Audit date: 2026-09-25. Scope: every `bjt_npn`/`bjt_pnp`, `diode` and
`nmos`/`pmos` card in `crates/hauksbee-models/db/{bjt,diodes,mosfet}.toml`
(12 + 37 + 23 = 72 cards). The parameters audited are the SPICE capacitances,
transit times and breakdown values the binder applies directly: `tf`, `tr`,
`cje`, `cjc`, `cjo`, `vj`, `m`, `tt`, `bv`, `ibv`, `cgs`, `cgd` (no card
carries `cbd`/`cbs`).

## Why

Several cards carried dynamic values that contradicted their own datasheets, and
cited sources that do not exist. The trigger was `bc847`: it cited a "SPICE model
appendix" that the Nexperia datasheet does not have, and its `cje = 0.9 pF` was
about 12x below the printed Ce typ of 11 pF.

## Rules applied

These are the relations `param_names::derivations` implements. A row reads its
typ, else the min/max midpoint, else its one printed bound, and the card comment
says which.

- **BJT:**
  - `tf = 1/(2π·fT)`. When only an fT min is printed, `tf` is an upper bound.
  - `cjc` comes from Cob/Cc and `cje` from Cib/Ce, taken to zero bias by
    `C·(1 + V/0.75)^0.33`.
  - The binder does not read `vje/mje/vjc/mjc`, so the engine defaults are the
    profile that actually runs. They are therefore the right profile to fold a
    printed row through.
- **MOSFET:** `cgd = Crss`, `cgs = Ciss − Crss`, both at the printed bias.
- **Diode:**
  - `cjo` comes from CT/CD/CJ at the lowest printed VR, taken to zero bias with
    the card's `vj/m`, else the defaults `vj = 1.0`, `m = 0.5`.
  - `bv/ibv` come from V(BR) or VZ and its test current. Where only VRRM/VR is
    printed, `bv` stays at that figure as a guaranteed lower bound on breakdown.
  - `tt = trr/ln(1 + IF/IR)`.
- **Vendor SPICE files:** a vendor file published on the product page is
  preferred, then cross-checked against the datasheet row as the engine
  evaluates it.
  - For a BJT that means with the default junction profile. Where the vendor
    CJC, evaluated that way, missed the printed Cc by more than 40%, the
    datasheet relation was used instead. This affects `bc857`, `bc807_40` and
    `bc807`.
- **Unsupported values:** a value that nothing printed supports is removed, so
  the binder default applies.
- **Schottky `tt`:** a Schottky's `tt` is removed (default 0). Its trr is
  capacitance- and fixture-limited, not stored charge.
- **`vj/m`:** kept only when fitted to a printed C-V figure or taken from a
  vendor file. The fits are least-squares fits to the figure's vector curve.
  When a fit degenerates (the curve flattens onto a package floor), the
  defaults are used.

## BJT (`db/bjt.toml`)

| card | param | old | new | source row and derivation |
|---|---|---|---|---|
| bc847 | cje | 9.0e-13 | 1.264e-11 | Nexperia BC847.txt CJE. Cross-check: BC847X_SER Rev. 13 Table 8, Ce typ 11 pF @ VEB = 0.5 V; the engine gives 10.7 pF |
| bc847 | cjc | 5.0e-13 | 3.624e-12 | BC847.txt CJC. Cross-check: Table 8, Cc max 1.5 pF @ VCB = 10 V; the engine gives 1.51 pF |
| bc847 | tf | 3.0e-10 | 6.258e-10 | BC847.txt TF (254 MHz). Consistent with Table 8 fT min 100 MHz @ VCE = 5 V, IC = 10 mA |
| bc847 | tr | none | 1.55e-7 | BC847.txt TR |
| 2n3904 | cje | 2.2e-12 | 4.642e-12 | onsemi 2n3904.lib CJE. Cross-check: 2N3904/D Rev. 3, Cibo max 8 pF @ VEB = 0.5 V; the engine gives 3.92 pF |
| 2n3904 | cjc | 7.306e-13 | 3.770e-12 | 2n3904.lib CJC. Cross-check: Cobo max 4 pF @ VCB = 5 V; the engine gives 1.92 pF |
| 2n3904 | tf | 3.011e-10 | 4.196e-10 | 2n3904.lib TF (379 MHz). Consistent with fT min 300 MHz |
| 2n3904 | tr | none | 6.820e-8 | 2n3904.lib TR |
| bc237 | cje | 1.2e-12 | 1.065e-11 | onsemi BC550/D Rev. 6, Cib typ 9 pF @ VEB = 0.5 V |
| bc237 | cjc | 5.0e-13 | 8.427e-12 | Cob typ 3.5 pF @ VCB = 10 V |
| bc237 | tf | 3.5e-10 | 5.305e-10 | fT typ 300 MHz @ VCE = 5 V, IC = 10 mA |
| s8050 | tf | 1.0e-9 | 1.061e-9 | Galaxy STM0043A, fT min 150 MHz @ VCE = 6 V, IC = 20 mA. An upper bound |
| s8050 | cje, cjc | none | none | No capacitance row or figure printed |
| bcm847bs | cje | 1.1e-11 | 1.244e-11 | Nexperia BCM847BS.txt CJE. Cross-check: v.8 Table 7, Ce typ 11 pF @ 0.5 V; the engine gives 10.5 pF |
| bcm847bs | cjc | 3.5e-12 | 3.347e-12 | BCM847BS.txt CJC. Cross-check: Table 7, Cc max 1.5 pF @ 10 V; the engine gives 1.39 pF |
| bcm847bs | tf | 6.4e-10 | 4.908e-10 | BCM847BS.txt TF (324 MHz). Table 7 fT typ is 250 MHz |
| bcm847bs | tr | none | 9.0e-8 | BCM847BS.txt TR |
| bc857 | cje | 9.0e-13 | 1.095e-11 | Nexperia BC857.txt CJE. No Ce row is printed |
| bc857 | cjc | 5.0e-13 | 1.083e-11 | BC856_BC857_BC858 Rev. 9 Table 8, Cc typ 4.5 pF @ VCB = −10 V. The vendor CJC gives 2.44 pF in the engine (−46%) |
| bc857 | tf | 3.0e-10 | 1.2e-9 | BC857.txt TF (133 MHz). Consistent with fT min 100 MHz |
| bc857 | tr | none | 8.8e-8 | BC857.txt TR |
| 2n3906 | cje | 2.2e-11 | 6.119e-12 | onsemi 2n3906.lib CJE. Cross-check: 2N3906/D Rev. 4, Cibo max 10 pF @ 0.5 V; the engine gives 5.17 pF |
| 2n3906 | cjc | 8.49e-13 | 6.850e-12 | 2n3906.lib CJC. Cross-check: Cobo max 4.5 pF @ 5 V; the engine gives 3.50 pF |
| 2n3906 | tf | 2.4e-10 | 5.218e-10 | 2n3906.lib TF (305 MHz). Consistent with fT min 250 MHz |
| 2n3906 | tr | none | 4.306e-7 | 2n3906.lib TR |
| bc307 | cje | 1.2e-12 | removed | onsemi BC556BTA/D Rev. 3 prints no Cib row |
| bc307 | cjc | 5.0e-13 | 1.445e-11 | Cob max 6 pF (the only printed bound) @ VCB = −10 V |
| bc307 | tf | 3.5e-10 | 1.061e-9 | fT typ 150 MHz @ VCE = −5 V, IC = −10 mA |
| bcm857bs | cje | 9.0e-13 | 1.14e-11 | Nexperia BCM857BS.txt CJE. Cross-check: v.9 Table 7, Ce typ 10 pF @ 0.5 V; the engine gives 9.63 pF |
| bcm857bs | cjc | 5.0e-13 | 5.839e-12 | BCM857BS.txt CJC. Cross-check: Cc max 2.2 pF @ 10 V; the engine gives 2.43 pF |
| bcm857bs | tf | 3.0e-10 | 9.0e-10 | BCM857BS.txt TF (177 MHz). Table 7 fT typ is 175 MHz |
| bcm857bs | tr | none | 1.0e-9 | BCM857BS.txt TR |
| bc817_40 | cje, cjc, tf, tr | vendor | unchanged | Match BC817-40.txt. Cc typ 3 pF @ 10 V; the engine gives 3.58 pF |
| bc807_40 | cje, tf, tr | vendor | unchanged | Match BC807-40.txt |
| bc807_40 | cjc | 1.786e-11 | 1.204e-11 | BC807_SER Rev. 8 Table 8, Cc typ 5 pF @ VCB = −10 V. The vendor CJC gives 7.42 pF in the engine (+48%) |
| bc807 | cje, tf, tr | vendor | unchanged | Match BC807.txt |
| bc807 | cjc | 2.004e-11 | 1.204e-11 | Same row. The vendor CJC (VJC = 0.14) gives 8.32 pF in the engine (+66%) |

## Diodes (`db/diodes.toml`)

| card | param | old | new | source row and derivation |
|---|---|---|---|---|
| 1n4148 | cjo | 4.0e-12 | unchanged | Vishay 81857 Rev. 1.6, CD max 4 pF @ VR = 0 V (already zero bias) |
| 1n4148 | vj, m | 0.8, 0.4 | removed | No C-V figure and no vendor file |
| 1n4148 | tt | 6.0e-9 | 1.154e-8 | trr max 8 ns @ IF = IR = 10 mA, iR = 1 mA: 8 ns / ln 2 |
| 1n4148 | bv, ibv | 100, none | 100, 1e-4 | V(BR) min 100 V @ IR = 100 µA |
| 1n4001..1n4007 | cjo | 2.0e-11 | 3.123e-11 | Vishay 88503 (29-Apr-2020), CJ typ 15 pF @ 4 V, taken to 0 V with the fitted vj/m |
| 1n4001..1n4007 | vj, m | 0.75, 0.333 | 0.4157, 0.3103 | Fit to Fig. 6, 2.3% rms |
| 1n4001..1n4007 | tt | 5.76e-6 | removed | No trr row in the Vishay sheet |
| 1n4001..1n4007 | bv | VRRM | unchanged | VRRM, a guaranteed lower bound; no V(BR) row |
| bat43 | cjo | 7.0e-12 | 9.899e-12 | Vishay 85660 Rev. 2.2, CD typ 7 pF @ VR = 1 V, at the defaults |
| bat43 | vj, m | 0.4, 0.4 | removed | The Fig. 4 fit is degenerate (package floor) |
| bat43 | tt | 1e-11 | removed | Schottky |
| bat43 | bv, ibv | 30, none | 30, 1e-4 | V(BR) min 30 V @ IR = 100 µA |
| bat54 | cjo | 1.0e-11 | 1.414e-11 | Nexperia BAT54 (1 Jul 2022) Table 7, Cd max 10 pF @ VR = 1 V, at the defaults |
| bat54 | vj, m, tt | 0.4, 0.4, 1e-11 | removed | Degenerate Fig. 3 fit; Schottky |
| bat54 | bv | 30 | unchanged | Table 5 VR, a lower bound |
| bat54_2pin | cjo | 1.0e-11 | 1.075e-11 | onsemi BAT54XV2T1/D Rev. 13, CT typ 7.6 pF @ VR = 1 V, at the defaults |
| bat54_2pin | vj, m, tt | 0.4, 0.4, 1e-11 | removed | As bat54 |
| bat54_2pin | bv, ibv | 30, none | 30, 1e-5 | V(BR)R min 30 V @ IR = 10 µA |
| usblc6_2 | bv, ibv | 6.0, none | 6.0, 1e-3 | ST DS4260 Rev. 7 Table 2, VBR min 6 V @ IR = 1 mA |
| schottky_generic | cjo, vj, m, tt, bv | 1.5e-12, 0.35, 0.4, 5e-12, 30 | removed | No part number and no source |
| led_red/green/blue/yellow/generic | cjo, vj, m, tt, bv | 2e-12, 1.7..2.8, 0.5, 1e-9, 5 | removed | No part number and no source. The 5 V reverse limit stays as a rating |
| 1n4728a..1n4759a | bv, ibv | VZ, IZT | unchanged | Multicomp V1.1 Specification Table, VZnom @ IZT; no capacitance or trr printed |
| 1n5817 | cjo | 1.25e-10 | 3.664e-10 | Vishay 88525 (01-Jul-2020), CJ typ 125 pF @ 4 V, taken to 0 V with the fitted vj/m |
| 1n5817 | vj, m | 0.4, 0.4 | 0.3838, 0.4415 | Fit to Fig. 5, 1.5% rms |
| 1n5818, 1n5819 | cjo | 1.1e-10 | 2.863e-10 | CJ typ 110 pF @ 4 V, taken to 0 V with the fitted vj/m |
| 1n5818, 1n5819 | vj, m | 0.4, 0.4 | 0.5017, 0.4359 | Fit to Fig. 6, 2.6% rms |
| 1n5817..1n5819 | tt | 1e-11 | removed | Schottky |
| 1n5822 | cjo | 2.0e-10 | 9.346e-10 | Taiwan Semi I2105, CJ typ 200 pF @ 4 V, taken to 0 V with the fitted vj/m |
| 1n5822 | vj, m | 0.4, 0.4 | 0.1094, 0.4252 | Fit to TSC Fig. 2, 2.6% rms. vj is a fit constant, not a barrier height |
| 1n5822 | tt | 1e-11 | removed | Schottky |
| 1n5819hw | cjo | 5.0e-11 | 1.122e-10 | Diodes DS30217 Rev. 22-2, CT typ 50 pF @ 4 V, taken to 0 V with the fitted vj/m |
| 1n5819hw | vj, m | 0.4, 0.4 | 1.051, 0.515 | Fit to Fig. 3, 4.5% rms |
| 1n5819hw | tt | 1e-11 | removed | Schottky |
| 1n5819hw | bv, ibv | 40 (as VRRM), none | 40, 1e-3 | V(BR)R min 40 V @ IR = 1 mA |
| bav70 | cjo | 1.5e-12 | unchanged | Nexperia BAV70 (1 Jul 2022) Table 7, Cd max 1.5 pF @ VR = 0 V |
| bav70 | vj, m | 0.7, 0.5 | removed | The Fig. 4 fit is degenerate |
| bav70 | tt | 4.0e-9 | 5.771e-9 | trr max 4 ns @ IF = IR = 10 mA: 4 ns / ln 2 |

## MOSFETs (`db/mosfet.toml`)

Every value is `cgd = Crss` and `cgs = Ciss − Crss` at the printed bias (typ
column) unless a vendor file is named.

| card | param | old | new | source row |
|---|---|---|---|---|
| 2n7002 | cgd, cgs | 3.0e-13, 5.0e-13 | 3.5e-12, 2.75e-11 | Nexperia 2N7002 Rev. 7 Table 6, Crss 3.5 / Ciss 31 pF @ VDS = 10 V |
| bss138 | cgd, cgs | 2.0e-13, 4.0e-13 | 6.0e-12, 2.1e-11 | Fairchild BSS138 Rev C(W), Crss 6 / Ciss 27 pF @ VDS = 25 V |
| ao3400a | cgd, cgs | 5.0e-11, 5.8e-10 | unchanged | AOS AO3400A Rev 3.1, Crss 50 / Ciss 630 pF @ VDS = 15 V |
| ao3401a | cgd, cgs | 6.0e-13, 1.0e-12 | 5.5e-11, 5.9e-10 | AOS AO3401A Rev 3.1, Crss 55 / Ciss 645 pF @ VDS = −15 V |
| cj2301 | cgd, cgs | 5.5e-11, 3.5e-10 | unchanged | JSCJ CJ2301 Rev. 2.1, Crss 55 / Ciss 405 pF @ VDS = −10 V |
| fds4435bz | cgd, cgs | 2.30e-10, 1.155e-9 | unchanged | Fairchild FDS4435BZ Rev. C1, Crss 230 / Ciss 1385 pF @ VDS = −15 V |
| ipa045n10n3g | cgd, cgs | none | 4.1e-11, 6.279e-9 | Infineon Rev. 2.4 Table 5, Crss 41 / Ciss 6320 pF @ VDS = 50 V |
| irf9358 | cgd, cgs | none | 2.4e-10, 1.5e-9 | Infineon IRF9358PbF Rev. 2.1, Crss 240 / Ciss 1740 pF @ VDS = −25 V |
| sir182dp | cgd, cgs | none | 5.0e-11, 3.2e-9 | Vishay doc 77800 Rev. B, Crss 50 / Ciss 3250 pF @ VDS = 30 V |
| generic_*_power_pkg | cgd, cgs | none | none | Placeholders with no datasheet |
| irf540n | cgd, cgs | 4.0e-11, 1.92e-9 | unchanged | IRF540NPbF Rev. 2.1, Crss 40 / Ciss 1960 pF @ VDS = 25 V |
| fdv301n | cgd, cgs | 1.3e-12, 8.2e-12 | unchanged | FDV301N/D Rev. 10, Crss 1.3 / Ciss 9.5 pF @ VDS = 10 V |
| dmg1012t | cgd, cgs | 5.37e-12, 5.53e-11 | unchanged | Diodes DS31783 Rev. 8-2, Crss 5.37 / Ciss 60.67 pF @ VDS = 16 V |
| pmv50enea | cgd, cgs | 4.2e-11, 2.34e-10 | unchanged | Nexperia v.1, Crss 42 / Ciss 276 pF @ VDS = 15 V. PMV50ENEA.lib CGS agrees |
| pmv50epea | cgd | 8.4e-11 | unchanged | Nexperia v.1, Crss 84 pF @ VDS = −15 V. The .lib CGD agrees |
| pmv50epea | cgs | 7.09e-10 | 7.507e-10 | PMV50EPEA.lib CGS. The datasheet gives 793 − 84 = 709 pF (−5.9%) |
| wpm2015 | cgd, cgs | 5.4e-11, 4.8e-10 | unchanged | WILLSEMI Rev. 1.2, Crss 54 / Ciss 534 pF @ VDS = −10 V |
| irlml6402 | cgd | 1.1e-10 | unchanged | IRLML6402 (4/28/2014), Crss 110 pF @ VDS = −10 V |
| irlml6402 | cgs | 5.23e-10 | 5.361e-10 | irlml6402.spi CGSO × W. The datasheet gives 633 − 110 = 523 pF (−2.5%) |
| ao3422 | cgd, cgs | 1.26e-11, 2.014e-10 | unchanged | AOS AO3422 Rev 2.1, Crss 12.6 / Ciss 214 pF @ VDS = 25 V |
| pmz550une | cgd | 4.2e-12 | unchanged | Nexperia v.1, Crss 4.2 pF @ VDS = 15 V |
| pmz550une | cgs | 2.61e-11 | 2.606e-11 | PMZ550UNE.lib CGS. The datasheet gives 30.3 − 4.2 = 26.1 pF |
| cj3134k | cgd, cgs | 9.0e-12, 7.0e-11 | unchanged | JSCJ CJ3134K Rev. 1.1, Crss 9 / Ciss 79 pF @ VDS = 16 V |
| dmg3404l | cgd, cgs | 5.1e-11, 5.9e-10 | unchanged | Diodes DS37640 Rev. 2-2, Crss 51 / Ciss 641 pF @ VDS = 15 V |
| fdn360p | cgd, cgs | 3.9e-11, 2.59e-10 | unchanged | FDN360P/D Rev. 8, Crss 39 / Ciss 298 pF @ VDS = −15 V |

## Sources

"Dev-set copy" means the PDF held under `target/launch/devset/pdf/`. The
revision printed in it is given.

**BJT**
- **BC847:** BC847X_SER Rev. 13, 1 Jul 2022 (dev-set copy).
  - SPICE: https://assets.nexperia.com/documents/spice-model/BC847.txt (extraction week 24/2021)
- **2N3904:** https://www.onsemi.com/download/data-sheet/pdf/2n3904-d.pdf (Rev. 3, Oct 2024).
  - SPICE: https://www.onsemi.com/download/models/lib/2n3904.lib (MODPEX, 7 Aug 2001)
- **2N3906:** https://www.onsemi.com/download/data-sheet/pdf/2n3906-d.pdf (Rev. 4, Feb 2010).
  - SPICE: https://www.onsemi.com/download/models/lib/2n3906.lib (MODPEX, 7 Aug 2001)
- **BC237/BC547 family:** https://www.onsemi.com/download/data-sheet/pdf/bc550-d.pdf (Rev. 6, Feb 2026)
- **BC307/BC557 family:** https://www.onsemi.com/download/data-sheet/pdf/bc556bta-d.pdf (Rev. 3, Jun 2024)
- **S8050:** Galaxy STM0043A, Aug 2020 (dev-set copy)
- **BCM847BS:** v.8 (dev-set copy).
  - SPICE: https://assets.nexperia.com/documents/spice-model/BCM847BS.txt (week 14/2006)
- **BC857:** https://assets.nexperia.com/documents/data-sheet/BC856_BC857_BC858.pdf (Rev. 9).
  - SPICE: https://assets.nexperia.com/documents/spice-model/BC857.txt (week 19/2020)
- **BCM857BS:** https://assets.nexperia.com/documents/data-sheet/BCM857BS.pdf (v.9).
  - SPICE: https://assets.nexperia.com/documents/spice-model/BCM857BS.txt (week 47/2005)
- **BC817-40, BC807-40, BC807:** BC817_SER and BC807_SER Rev. 8 (dev-set copies).
  - SPICE: BC817-40.txt, BC807-40.txt and BC807.txt, from the same Nexperia spice-model path

**Diodes**
- **1N4148:** https://www.vishay.com/docs/81857/1n4148.pdf (Rev. 1.6, 07-Nov-2024)
- **1N4001..1N4007:** https://www.vishay.com/docs/88503/1n4001.pdf (29-Apr-2020).
  - onsemi 1n4001rl.lib was checked and not used: its CJO/VJ/M give 3.9 pF at 4 V, against the 15 pF row.
- **BAT43:** Vishay 85660 Rev. 2.2 (dev-set copy)
- **BAT54:** Nexperia BAT54, 1 Jul 2022 (dev-set copy)
- **BAT54XV2:** https://www.onsemi.com/pdf/datasheet/bat54xv2t1-d.pdf (Rev. 13)
- **USBLC6-2:** ST DS4260 Rev. 7 (dev-set copy)
- **1N4728A..1N4764A:** Multicomp V1.1 (dev-set copy)
- **1N5817..1N5819:** Vishay 88525 (dev-set copy)
- **1N5822:**
  - https://www.vishay.com/docs/88526/1n5820.pdf
  - https://www.taiwansemi.com/assets/datasheet/pdf.php?pn=1N5822 (I2105)
- **1N5819HW:** Diodes DS30217 Rev. 22-2 (dev-set copy)
- **BAV70:** Nexperia BAV70, 1 Jul 2022 (dev-set copy)

**MOSFETs**
- **2N7002:** https://assets.nexperia.com/documents/data-sheet/2N7002.pdf
- **BSS138:** Fairchild Rev C(W), via https://cdn.sparkfun.com/datasheets/BreakoutBoards/BSS138.pdf
- **AO3401A:** https://www.aosmd.com/res/datasheets/AO3401A.pdf
- **AO3422:** https://www.aosmd.com/res/datasheets/AO3422.pdf
- **CJ2301:** https://evelta.com/content/datasheets/217-CJ2301.pdf
- **FDS4435BZ:** https://www.farnell.com/datasheets/673294.pdf
- **IPA045N10N3 G:** https://www.infineon.com/dgdl/Infineon-IPA045N10N3+G-DS-v02_04-EN.pdf?fileId=db3a30432239cccd0122600237f57ece
- **IRF9358:** https://www.infineon.com/dgdl/irf9358pbf.pdf?fileId=5546d462533600a4015355fb6c9a1a36
- **SiR182DP:** https://www.vishay.com/docs/77800/sir182dp.pdf
- **DMG1012T:** https://www.diodes.com/assets/Datasheets/DMG1012T.pdf
- **PMV50ENEA, PMV50EPEA, PMZ550UNE:**
  - Datasheets: https://assets.nexperia.com/documents/data-sheet/<PART>.pdf
  - SPICE: https://assets.nexperia.com/documents/spice-model/<PART>.lib
- **IRLML6402 SPICE:** https://www.infineon.com/dgdl/irlml6402.spi?fileId=5546d462533600a401535683a6e32bcc
- **WPM2015:** https://datasheet.lcsc.com/datasheet/pdf/903395ae1f2101a77730f1a7221883cd.pdf?productCode=C213159
- **CJ3134K:** https://www.jscj-elec.com/gallery/file/CJ3134K%20SOT-23%20V1.1.pdf
- **AO3400A, IRF540N, FDV301N, IRLML6402, DMG3404L, FDN360P:** dev-set copies

## Citations found to point at nothing

These are reworded on the cards. The DC values they covered are now labelled
legacy where no source could be found, and are not changed here.

**BJT**
- **bc847, bc857:** "datasheet … SPICE model appendix". Neither datasheet has an appendix.
- **2n3904, 2n3906:** "ON Semiconductor … SPICE model, Rev. 7". There is no Rev. 7. The DC values match the PSpice-library Q2N3904/Q2N3906 cards, not onsemi's `.lib`.
  - The ngspice deck `bjt_sgp_high_injection.cir` made the same misattribution and is corrected.
- **bc237, bc307:** "Fairchild … datasheet, SPICE parameters". Those datasheets print none.
- **bcm847bs:** "Rev. 4 2022, SPICE model (same as BC847)". That revision does not exist, and the vendor model is not BC847's.
- **bcm857bs:** the DC values are copied from bc857, and its datasheet supports none of them.

**Diodes**
- **1n4148:** "Vishay … Rev. 14, SPICE model parameters". The sheet is Rev. 1.6 and has no SPICE section.
- **1n4001..1n4007:** "Vishay Rev. 15, SPICE model" and "ON Semiconductor SPICE model". Neither exists with these values.

**MOSFETs**
- **bss138:** "Nexperia BSS138 Rev. 5, SPICE model". Nexperia sells only BSS138P/BK variants, and the card is the Fairchild die.
- **ao3401a:** "datasheet, SPICE parameters". None are printed.
  - Legacy vto = −1.1 V against a typ of −0.9 V.
  - Legacy rd + rs = 100 mΩ against an RDS(on) typ of 41 mΩ.
- **ipa045n10n3g:** "Rev 2.6 2019-02-15, Table 1 & SPICE". No such revision, and no SPICE section.
- **sir182dp:** "Document No. 66664 Rev. E". The real document is 77800 Rev. B.

## Left open

These are DC or ratings issues found during the audit and outside its scope:

- **sir182dp:** the real part is 60 V, with RDS(on) typ 2.3 mΩ and ID 117 A. Its datasheet pinout is S 1-3, G 4, D 5-8, which the card's pin map disagrees with.
- **ipa045n10n3g:** a TO-220 FullPAK. ID is 64 A, VGS(th) is 2/2.7/3.5 V and RDS(on) typ is 3.9 mΩ; the card's values differ.
- **irf9358:** `vgs_th_max` should be 2.4 V.
- **cj3134k:** kp/rd/rs are fitted to the RDS(on) max column, although Rev. 1.1 does print a typ column.
- **1N4001..1N4007:** the Vishay sheet prints no trr, so reverse recovery (`tt`) is now off. A vendor trr row would restore it.
- **2n3904, 2n3906, bc847, bc857, bcm8x7bs:** the capacitances and transit times now come from vendor files, but the DC parameters are still legacy.
