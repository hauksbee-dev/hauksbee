#!/usr/bin/env python3
"""Generate cane.kicad_pcb: a synthetic 'ultrasonic walking cane' board with
deliberate quirks, for the Hauksbee end-to-end measurement pass.

Planted quirks (see README.md next to this file):
  Q1  SDA has no pull-up (SCL has R1 4.7k)                       -> MissingI2cPullup
  Q2  MOSFET gate MOTOR_GATE has no pull-down (floats at reset)  -> boot advisory (needs fw)
  Q3  0.15 mm trace on the VBAT motor path (TP4056 cites 1 A)    -> ampacity
  Q4  JP1 battery-sense jumper is DNP                             -> DNP handling
  Q5  HC-SR04 (5 V module) VCC pin fed from +3V3                  -> envelope (needs model)
Bonus (not planted on purpose but real): AMS1117-3.3 fed from a 1S Li-ion.
"""

NETS = ["", "VBAT", "GND", "+3V3", "+5V_USB", "MOTOR_GATE", "MOTOR_N", "TRIG", "ECHO",
        "SDA", "SCL", "BTN1", "BTN2", "BUZZ", "PROG", "VBAT_SENSE_IN", "VBAT_SENSE",
        "CHRG_LED", "RESET", "LED_A"]
NID = {n: i for i, n in enumerate(NETS)}

comps = []  # (ref, value, footprint, x, y, rot, attrs, pads)  pad = (num, kind, shape, dx, dy, w, h, drill, net)


def smd(num, dx, dy, w, h, net):
    return (str(num), "smd", "roundrect", dx, dy, w, h, None, net)


def tht(num, dx, dy, d, drill, net):
    return (str(num), "thru_hole", "circle", dx, dy, d, d, drill, net)


def two_pad_0805(ref, value, x, y, n1, n2, fp="Resistor_SMD:R_0805_2012Metric"):
    comps.append((ref, value, fp, x, y, 0, "smd", [smd(1, -0.95, 0, 1.0, 1.4, n1), smd(2, 0.95, 0, 1.0, 1.4, n2)]))


# ---- power input / charger / LDO -------------------------------------------------
comps.append(("BT1", "Li-ion 1S 18650", "Battery:BatteryHolder_Keystone_1042_1x18650", 5, 9, 0, "through_hole",
              [tht(1, 0, -1, 1.8, 1.0, "VBAT"), tht(2, 0, 1, 1.8, 1.0, "GND")]))
two_pad_0805("C1", "10u", 10, 8, "VBAT", "GND", "Capacitor_SMD:C_0805_2012Metric")
comps.append(("U2", "AMS1117-3.3", "Package_TO_SOT_SMD:SOT-223-3_TabPin2", 16, 8, 0, "smd",
              [smd(1, -2.3, 3.2, 1.0, 1.5, "GND"), smd(2, 0, 3.2, 1.0, 1.5, "+3V3"), smd(3, 2.3, 3.2, 1.0, 1.5, "VBAT"),
               smd(4, 0, -3.2, 3.3, 1.5, "+3V3")]))
two_pad_0805("C2", "10u", 22, 8, "+3V3", "GND", "Capacitor_SMD:C_0805_2012Metric")
# Q4: DNP battery-sense jumper.
comps.append(("JP1", "SolderJumper_Open", "Jumper:SolderJumper-2_P1.3mm_Open_RoundedPad1.0x1.5mm", 5, 14, 0, "smd dnp",
              [smd(1, -0.65, 0, 1.0, 1.5, "VBAT"), smd(2, 0.65, 0, 1.0, 1.5, "VBAT_SENSE_IN")]))
two_pad_0805("R6", "100k", 10, 14, "VBAT_SENSE_IN", "VBAT_SENSE")
two_pad_0805("R7", "100k", 16, 14, "VBAT_SENSE", "GND")

# ---- motor driver: Q2 gate has no pull-down; Q3 narrow trace on VBAT motor path ----
comps.append(("M1", "Vibration_Motor_3V", "Motor:Motor_DC_Coin_10mm", 5, 22, 0, "through_hole",
              [tht(1, 0, -1, 1.5, 0.8, "VBAT"), tht(2, 0, 1, 1.5, 0.8, "MOTOR_N")]))
comps.append(("D1", "1N4148W", "Diode_SMD:D_SOD-123", 10, 22, 0, "smd",
              [smd("A", -1.3, 0, 0.9, 1.2, "MOTOR_N"), smd("K", 1.3, 0, 0.9, 1.2, "VBAT")]))
comps.append(("Q1", "AO3400A", "Package_TO_SOT_SMD:SOT-23", 16, 22, 0, "smd",
              [smd(1, -0.95, 1, 0.6, 1.0, "MOTOR_GATE"), smd(2, 0.95, 1, 0.6, 1.0, "GND"), smd(3, 0, -1, 0.6, 1.0, "MOTOR_N")]))

# ---- MCU -------------------------------------------------------------------------
MCU_NETS = {7: "+3V3", 8: "GND", 20: "+3V3", 22: "GND", 21: None,
            14: "MOTOR_GATE", 15: "BUZZ", 4: "BTN1", 5: "BTN2",
            24: "TRIG", 23: "ECHO", 27: "SDA", 28: "SCL", 26: "VBAT_SENSE", 1: "RESET"}
mcu_pads = []
for i in range(1, 33):
    if i <= 8:
        dx, dy, w, h = -3.9, -2.8 + 0.8 * (i - 1), 1.2, 0.5
    elif i <= 16:
        dx, dy, w, h = -2.8 + 0.8 * (i - 9), 3.9, 0.5, 1.2
    elif i <= 24:
        dx, dy, w, h = 3.9, 2.8 - 0.8 * (i - 17), 1.2, 0.5
    else:
        dx, dy, w, h = 2.8 - 0.8 * (i - 25), -3.9, 0.5, 1.2
    mcu_pads.append(smd(i, round(dx, 3), round(dy, 3), w, h, MCU_NETS.get(i)))
comps.append(("U1", "ATmega328P", "Package_QFP:TQFP-32_7x7mm_P0.8mm", 30, 20, 0, "smd", mcu_pads))
two_pad_0805("C3", "100n", 22, 17, "+3V3", "GND", "Capacitor_SMD:C_0603_1608Metric")
two_pad_0805("C4", "100n", 37, 20.4, "+3V3", "GND", "Capacitor_SMD:C_0603_1608Metric")
two_pad_0805("R8", "10k", 23, 25, "+3V3", "RESET")
comps.append(("BZ1", "Piezo_Buzzer", "Buzzer_Beeper:Buzzer_12x9.5RM7.6", 10, 32, 0, "through_hole",
              [tht(1, -1, 0, 1.5, 0.8, "BUZZ"), tht(2, 1, 0, 1.5, 0.8, "GND")]))

# ---- IMU (I2C): Q1 SDA lacks pull-up ---------------------------------------------
IMU_NETS = {1: "GND", 2: "SDA", 3: "+3V3", 7: "+3V3", 8: "GND", 9: "GND", 10: "+3V3", 12: "SCL"}
imu_pads = []
for i in range(1, 13):
    if i <= 6:
        dx, dy = -0.85, -1.25 + 0.5 * (i - 1)
    else:
        dx, dy = 0.85, 1.25 - 0.5 * (i - 7)
    imu_pads.append(smd(i, dx, round(dy, 3), 0.5, 0.3, IMU_NETS.get(i)))
comps.append(("U4", "BMA423", "Package_LGA:Bosch_LGA-12_2x2mm_P0.5mm", 31, 9, 180, "smd", imu_pads))
two_pad_0805("R1", "4.7k", 27, 7, "+3V3", "SCL")
two_pad_0805("C5", "100n", 36, 9, "+3V3", "GND", "Capacitor_SMD:C_0603_1608Metric")

# ---- Q5: HC-SR04 header, VCC on +3V3 --------------------------------------------
comps.append(("J2", "HC-SR04", "Connector_PinHeader_2.54mm:PinHeader_1x04_P2.54mm_Vertical", 40, 4, 0, "through_hole",
              [tht(1, -3.81, 0, 1.7, 1.0, "+3V3"), tht(2, -1.27, 0, 1.7, 1.0, "TRIG"),
               tht(3, 1.27, 0, 1.7, 1.0, "ECHO"), tht(4, 3.81, 0, 1.7, 1.0, "GND")]))

# ---- buttons with pull-ups -------------------------------------------------------
two_pad_0805("R2", "10k", 50, 8, "+3V3", "BTN1")
two_pad_0805("R3", "10k", 50, 14, "+3V3", "BTN2")
comps.append(("SW1", "SW_Push", "Button_Switch_SMD:SW_SPST_PTS645", 56, 8, 0, "smd",
              [smd(1, -1, 0, 1.0, 1.4, "BTN1"), smd(2, 1, 0, 1.0, 1.4, "GND")]))
comps.append(("SW2", "SW_Push", "Button_Switch_SMD:SW_SPST_PTS645", 56, 14, 0, "smd",
              [smd(1, -1, 0, 1.0, 1.4, "BTN2"), smd(2, 1, 0, 1.0, 1.4, "GND")]))

# ---- USB + charger ----------------------------------------------------------------
comps.append(("J3", "USB_Micro-B", "Connector_USB:USB_Micro-B_Molex-105017-0001", 50, 34, 0, "smd",
              [smd(1, -1, 0, 0.8, 1.4, "+5V_USB"), smd(2, -0.5, 0, 0.3, 1.4, None), smd(3, 0, 0, 0.3, 1.4, None),
               smd(4, 0.5, 0, 0.3, 1.4, None), smd(5, 1, 0, 0.8, 1.4, "GND")]))
two_pad_0805("C6", "1u", 36, 38, "+5V_USB", "GND", "Capacitor_SMD:C_0603_1608Metric")
tp_pads = []
TP_NETS = {1: "GND", 2: "PROG", 3: "GND", 4: "+5V_USB", 5: "VBAT", 6: None, 7: "CHRG_LED", 8: "+5V_USB"}
for i in range(1, 9):
    if i <= 4:
        dx, dy = -1.905 + 1.27 * (i - 1), 2.6
    else:
        dx, dy = 1.905 - 1.27 * (i - 5), -2.6
    tp_pads.append(smd(i, round(dx, 3), dy, 0.6, 1.5, TP_NETS.get(i)))
comps.append(("U3", "TP4056", "Package_SO:SOIC-8_3.9x4.9mm_P1.27mm", 42, 36, 0, "smd", tp_pads))
two_pad_0805("R4", "1.2k", 47, 41.2, "PROG", "GND")   # 1200/1.2k = 1.0 A charge current
two_pad_0805("R5", "1k", 54, 31, "+5V_USB", "LED_A")
comps.append(("D2", "LED_RED", "LED_SMD:LED_0805_2012Metric", 54, 27, 0, "smd",
              [smd("A", 0.95, 0, 1.0, 1.4, "LED_A"), smd("K", -0.95, 0, 1.0, 1.4, "CHRG_LED")]))

# ---- routed copper (partial; enough to carry the planted trace-width quirk) -------
W = 0.25
segs = [
    # VBAT distribution (wide) and the deliberately narrow motor feed (Q3)
    ("VBAT", 0.5, [(5, 8), (5, 6.5), (18.3, 6.5), (18.3, 11.2)]),
    ("VBAT", 0.5, [(9.05, 8), (9.05, 6.5)]),
    ("VBAT", 0.3, [(5, 8), (3, 8), (3, 14), (4.35, 14)]),
    ("VBAT", 0.15, [(4.35, 14), (4.35, 21), (5, 21)]),           # Q3: 0.15 mm, ~1 A cited
    ("VBAT", 0.15, [(5, 21), (5, 19.5), (11.3, 19.5), (11.3, 22)]),
    ("GND", 0.5, [(10.95, 8), (10.95, 11.2), (13.7, 11.2)]),
    ("GND", 0.5, [(5, 10), (5, 11.5), (10.95, 11.5)]),
    ("+3V3", 0.4, [(16, 11.2), (16, 12.6), (21.05, 12.6), (21.05, 8)]),
    ("+3V3", 0.4, [(21.05, 12.6), (21.05, 17)]),
    ("+3V3", 0.4, [(21.05, 17), (21.05, 22), (26.1, 22)]),
    ("+3V3", 0.3, [(22.05, 22), (22.05, 25)]),
    ("VBAT_SENSE_IN", W, [(5.65, 14), (9.05, 14)]),
    ("VBAT_SENSE", W, [(10.95, 14), (15.05, 14)]),
    ("MOTOR_N", 0.15, [(5, 23), (5, 24.5), (8.7, 24.5), (8.7, 22)]),
    ("MOTOR_N", 0.15, [(16, 21), (16, 19.5), (13, 19.5), (13, 24.5), (8.7, 24.5)]),
    ("MOTOR_GATE", 0.2, [(31.2, 23.9), (31.2, 26.5), (15.05, 26.5), (15.05, 23)]),
    ("BUZZ", 0.2, [(32, 23.9), (32, 30), (9, 30), (9, 32)]),
    ("SDA", 0.2, [(31.2, 16.1), (31.2, 13), (33.5, 13), (33.5, 9.75), (31.85, 9.75)]),
    ("SCL", 0.2, [(30.4, 16.1), (30.4, 13), (27.95, 13), (27.95, 7)]),
    ("SCL", 0.2, [(30.15, 13), (30.15, 10.25)]),
    ("TRIG", 0.2, [(33.9, 17.2), (38, 17.2), (38, 12), (38.73, 12), (38.73, 4)]),
    ("ECHO", 0.2, [(33.9, 18.0), (39.5, 18.0), (39.5, 13), (41.27, 13), (41.27, 4)]),
    ("BTN1", W, [(50.95, 8), (55, 8)]),
    ("BTN2", W, [(50.95, 14), (55, 14)]),
    ("PROG", 0.2, [(41.365, 38.6), (41.365, 41.2), (46.05, 41.2)]),
    ("+5V_USB", 0.4, [(49, 34), (49, 39.5), (43.905, 39.5), (43.905, 38.6)]),
    ("+5V_USB", 0.4, [(53.05, 31), (53.05, 32.5), (49, 32.5), (49, 34)]),
    ("CHRG_LED", 0.2, [(41.365, 33.4), (41.365, 29), (53.05, 29), (53.05, 27)]),
    ("LED_A", 0.2, [(54.95, 31), (54.95, 27)]),
]

out = []
out.append('(kicad_pcb (version 20241229) (generator "pcbnew") (generator_version "9.0")')
out.append('  (general (thickness 1.6) (legacy_teardrops no))')
out.append('  (paper "A4")')
out.append('  (title_block (title "Ultrasonic walking cane, synthetic quirk board") (rev "A"))')
out.append('  (layers (0 "F.Cu" signal) (31 "B.Cu" signal) (36 "B.SilkS" user) (37 "F.SilkS" user)'
           ' (38 "B.Mask" user) (39 "F.Mask" user) (44 "Edge.Cuts" user) (48 "B.Fab" user) (49 "F.Fab" user)'
           ' (34 "B.Paste" user) (35 "F.Paste" user))')
out.append('  (setup (pad_to_mask_clearance 0) (min_clearance 0.2))')
for i, n in enumerate(NETS):
    out.append(f'  (net {i} "{n}")')

for ref, value, fp, x, y, rot, attrs, pads in comps:
    out.append(f'  (footprint "{fp}" (layer "F.Cu") (at {x} {y} {rot}) (attr {attrs})')
    out.append(f'    (property "Reference" "{ref}" (at 0 -2 0) (layer "F.SilkS"))')
    out.append(f'    (property "Value" "{value}" (at 0 2 0) (layer "F.Fab"))')
    for num, kind, shape, dx, dy, w, h, drill, net in pads:
        layers = '"*.Cu" "*.Mask"' if kind == "thru_hole" else '"F.Cu" "F.Paste" "F.Mask"'
        d = f" (drill {drill})" if drill else ""
        n = f' (net {NID[net]} "{net}")' if net else ""
        out.append(f'    (pad "{num}" {kind} {shape} (at {dx} {dy}) (size {w} {h}){d} (layers {layers}){n})')
    out.append('  )')

for net, w, pts in segs:
    for (x0, y0), (x1, y1) in zip(pts, pts[1:]):
        out.append(f'  (segment (start {x0} {y0}) (end {x1} {y1}) (width {w}) (layer "F.Cu") (net {NID[net]}))')

# outline 60 x 44
for (x0, y0, x1, y1) in [(0, 0, 60, 0), (60, 0, 60, 44), (60, 44, 0, 44), (0, 44, 0, 0)]:
    out.append(f'  (gr_line (start {x0} {y0}) (end {x1} {y1}) (layer "Edge.Cuts") (width 0.1))')
out.append(')')

import sys
path = sys.argv[1] if len(sys.argv) > 1 else "cane.kicad_pcb"
open(path, "w").write("\n".join(out) + "\n")
print(f"wrote {path}: {len(comps)} footprints, {len(NETS)-1} nets, {sum(len(p)-1 for _,_,p in segs)} segments")
