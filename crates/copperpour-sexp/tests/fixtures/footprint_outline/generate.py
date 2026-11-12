"""Generate footprint-owned Edge.Cuts fixtures with KiCad's own pcbnew API.

usage: python3 gen_fixture.py OUT_DIR
"""
import math, sys, os
import pcbnew as P

MM = P.FromMM
def v(x, y): return P.VECTOR2I(MM(x), MM(y))

def seg(fp, layer, a, b):
    s = P.PCB_SHAPE(fp, P.SHAPE_T_SEGMENT); s.SetLayer(layer); s.SetWidth(MM(0.1))
    s.SetStart(v(*a)); s.SetEnd(v(*b)); fp.Add(s)

def arc(fp, layer, a, m, b):
    s = P.PCB_SHAPE(fp, P.SHAPE_T_ARC); s.SetLayer(layer); s.SetWidth(MM(0.1))
    s.SetArcGeometry(v(*a), v(*m), v(*b)); fp.Add(s)

def rounded_rect(fp, layer, cx, cy, w, h, r):
    x0, y0, x1, y1 = cx - w/2, cy - h/2, cx + w/2, cy + h/2
    k = r * (1 - math.sqrt(0.5))
    seg(fp, layer, (x0 + r, y0), (x1 - r, y0))
    arc(fp, layer, (x1 - r, y0), (x1 - k, y0 + k), (x1, y0 + r))
    seg(fp, layer, (x1, y0 + r), (x1, y1 - r))
    arc(fp, layer, (x1, y1 - r), (x1 - k, y1 - k), (x1 - r, y1))
    seg(fp, layer, (x1 - r, y1), (x0 + r, y1))
    arc(fp, layer, (x0 + r, y1), (x0 + k, y1 - k), (x0, y1 - r))
    seg(fp, layer, (x0, y1 - r), (x0, y0 + r))
    arc(fp, layer, (x0, y0 + r), (x0 + k, y0 + k), (x0 + r, y0))

def rect(fp, layer, x0, y0, x1, y1):
    s = P.PCB_SHAPE(fp, P.SHAPE_T_RECTANGLE); s.SetLayer(layer); s.SetWidth(MM(0.05))
    s.SetStart(v(x0, y0)); s.SetEnd(v(x1, y1)); fp.Add(s)

def mech(board, ref):
    fp = P.FOOTPRINT(board); fp.SetReference(ref); fp.SetValue("MECH")
    fp.SetFPID(P.LIB_ID("fixture", "MECH_outline")); fp.SetLayer(P.F_Cu)
    board.Add(fp); fp.SetPosition(v(0, 0))
    # outer contour 140 x 100, r = 5 corners -> 4 lines + 4 arcs
    rounded_rect(fp, P.Edge_Cuts, 0, 0, 140, 100, 5)
    # internal cutout 24.25 x 7.333 at local (20, 10), r = 1 -> 4 lines + 4 arcs
    rounded_rect(fp, P.Edge_Cuts, 20, 10, 24.25, 7.333, 1)
    # a courtyard over the whole outline: the overlap trap from the issue
    rect(fp, P.F_CrtYd, -70.25, -50.25, 70.25, 50.25)
    return fp

def resistor(board, ref, x, y, net):
    fp = P.FOOTPRINT(board); fp.SetReference(ref); fp.SetValue("10k")
    fp.SetFPID(P.LIB_ID("fixture", "R_0603")); fp.SetLayer(P.F_Cu)
    board.Add(fp); fp.SetPosition(v(0, 0))
    for num, px in (("1", -0.8), ("2", 0.8)):
        p = P.PAD(fp); p.SetNumber(num); p.SetShape(P.PAD_SHAPE_RECTANGLE)
        p.SetAttribute(P.PAD_ATTRIB_SMD); p.SetLayerSet(P.PAD.SMDMask())
        p.SetSize(P.F_Cu, v(0.8, 0.95)); p.SetPosition(v(px, 0)); p.SetNet(net); fp.Add(p)
    rect(fp, P.F_CrtYd, -1.5, -0.75, 1.5, 0.75)
    fp.SetPosition(v(x, y))
    return fp

def build(path, x, y, rot, back):
    b = P.BOARD()
    net = P.NETINFO_ITEM(b, "SIG"); b.Add(net)
    m = mech(b, "MECH1")
    m.SetPosition(v(x, y)); m.SetOrientationDegrees(rot)
    if back:
        m.Flip(m.GetPosition(), P.FLIP_DIRECTION_TOP_BOTTOM)
    resistor(b, "R1", x - 10, y - 20, net)
    resistor(b, "R2", x + 10, y - 20, net)
    P.SaveBoard(path, b)
    b2 = P.LoadBoard(path)
    bb = b2.GetBoardEdgesBoundingBox()
    polys = P.SHAPE_POLY_SET()
    ok = b2.GetBoardPolygonOutlines(polys, True)
    print(os.path.basename(path), "edges bbox mm:",
          [round(P.ToMM(t), 6) for t in (bb.GetLeft(), bb.GetTop(), bb.GetRight(), bb.GetBottom())],
          "outline ok:", ok, "outlines:", polys.OutlineCount(),
          "holes:", [polys.HoleCount(i) for i in range(polys.OutlineCount())])

out = sys.argv[1]
build(os.path.join(out, "footprint_outline_front.kicad_pcb"), 100, 100, 0, False)
build(os.path.join(out, "footprint_outline_back_rot30.kicad_pcb"), 100, 100, 30, True)
