import AppKit
import Foundation

// The app icon: the Graphicle mark (Apex) on the brand's dark background.
// The numbers are the brand kit's mark-geometry.json: variants.full (a 128-unit
// box, y growing downward) and colours.dark. The box is drawn at 5 px a unit and
// moved by the mark's optical offset (half the distance from the code nodes'
// centroid to the box centre), as the kit's 1024 px avatar places it.

struct Node { let x: CGFloat, y: CGFloat, r: CGFloat }
struct Edge { let x1: CGFloat, y1: CGFloat, x2: CGFloat, y2: CGFloat }
// A link's gradient runs from the code colour at its corner end (x2, y2) to the memory violet at (x1, y1).
struct Link { let edge: Edge, corner: UInt32 }

let codeNodes = [Node(x: 51.59, y: 24.12, r: 9.5), Node(x: 110.08, y: 93.24, r: 9.5), Node(x: 17.92, y: 103.88, r: 9.5)]
let memoryNode = Node(x: 58.68, y: 75.52, r: 14)
// Edges are already trimmed short of their nodes; all are 10 wide with round caps, drawn under the nodes.
let codeEdges = [Edge(x1: 63.11, y1: 37.74, x2: 98.56, y2: 79.63),
                 Edge(x1: 92.36, y1: 95.29, x2: 35.64, y2: 101.83),
                 Edge(x1: 24.86, y1: 87.45, x2: 44.66, y2: 40.55)]
let links = [Link(edge: Edge(x1: 55.63, y1: 53.4, x2: 54.03, y2: 41.79), corner: 0x36B4F4),
             Link(edge: Edge(x1: 79.8, y1: 82.8, x2: 93.22, y2: 87.43), corner: 0x3AAFF5),
             Link(edge: Edge(x1: 40.35, y1: 88.27, x2: 32.56, y2: 93.69), corner: 0x5881FE)]
let edgeWidth: CGFloat = 10
let background: UInt32 = 0x0A0B0F
// One code gradient across the whole mark, shared by every code node and code edge.
let codeStart: UInt32 = 0x5B7CFF, codeEnd: UInt32 = 0x22D3EE
let codeFrom = CGPoint(x: 16, y: 112), codeTo = CGPoint(x: 112, y: 16)
let violet: UInt32 = 0xA78BFA

let sRGB = CGColorSpace(name: CGColorSpace.sRGB)!
func color(_ hex: UInt32) -> CGColor {
    let parts: [CGFloat] = [CGFloat((hex >> 16) & 0xFF) / 255, CGFloat((hex >> 8) & 0xFF) / 255, CGFloat(hex & 0xFF) / 255, 1]
    return CGColor(colorSpace: sRGB, components: parts)!
}
func gradient(_ start: UInt32, _ end: UInt32) -> CGGradient {
    let locations: [CGFloat] = [0, 1]
    return CGGradient(colorsSpace: sRGB, colors: [color(start), color(end)] as CFArray, locations: locations)!
}
func stroke(_ edge: Edge, width: CGFloat, with fill: CGGradient, from start: CGPoint, to end: CGPoint, in context: CGContext) {
    context.saveGState()
    context.setLineWidth(width)
    context.setLineCap(.round)
    context.move(to: CGPoint(x: edge.x1, y: edge.y1))
    context.addLine(to: CGPoint(x: edge.x2, y: edge.y2))
    context.replacePathWithStrokedPath()
    context.clip()
    context.drawLinearGradient(fill, start: start, end: end, options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
    context.restoreGState()
}
func circle(_ node: Node) -> CGRect {
    CGRect(x: node.x - node.r, y: node.y - node.r, width: 2 * node.r, height: 2 * node.r)
}

let size = NSSize(width: 1024, height: 1024)
let image = NSImage(size: size)
image.lockFocus()
guard let context = NSGraphicsContext.current?.cgContext else { fatalError("Icon rendering failed") }
context.setFillColor(color(background))
context.addPath(CGPath(roundedRect: CGRect(x: 0, y: 0, width: 1024, height: 1024), cornerWidth: 220, cornerHeight: 220, transform: nil))
context.fillPath()

let scale: CGFloat = 5
let centroid = CGPoint(x: codeNodes.reduce(CGFloat(0)) { $0 + $1.x } / 3, y: codeNodes.reduce(CGFloat(0)) { $0 + $1.y } / 3)
let optical = CGPoint(x: (64 - centroid.x) / 2, y: (64 - centroid.y) / 2)
context.saveGState()
// Box units with y down onto the unflipped canvas: the box centre lands at (512, 512) plus the optical offset.
context.translateBy(x: 512 - scale * (64 - optical.x), y: 512 + scale * (64 - optical.y))
context.scaleBy(x: scale, y: -scale)
let code = gradient(codeStart, codeEnd)
for edge in codeEdges {
    stroke(edge, width: edgeWidth, with: code, from: codeFrom, to: codeTo, in: context)
}
for link in links {
    stroke(link.edge, width: edgeWidth, with: gradient(link.corner, violet),
           from: CGPoint(x: link.edge.x2, y: link.edge.y2), to: CGPoint(x: link.edge.x1, y: link.edge.y1), in: context)
}
context.saveGState()
for node in codeNodes {
    context.addEllipse(in: circle(node))
}
context.clip()
context.drawLinearGradient(code, start: codeFrom, end: codeTo, options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
context.restoreGState()
context.setFillColor(color(violet))
context.fillEllipse(in: circle(memoryNode))
context.restoreGState()
image.unlockFocus()
guard let tiff = image.tiffRepresentation, let bitmap = NSBitmapImageRep(data: tiff),
      let data = bitmap.representation(using: .png, properties: [:]) else { fatalError("Icon rendering failed") }
try data.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
