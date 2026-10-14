import Foundation
import AppKit
import Vision
import PDFKit
import ImageIO

enum DocumentError: Error { case invalid, tooLarge }
let limit = 512 * 1024
func recognize(_ image: CGImage) throws -> String {
    let request = VNRecognizeTextRequest()
    request.recognitionLevel = .accurate
    request.usesLanguageCorrection = true
    try VNImageRequestHandler(cgImage: image).perform([request])
    return (request.results ?? []).compactMap { $0.topCandidates(1).first?.string }.joined(separator: "\n")
}
do {
    guard CommandLine.arguments.count == 2 else { throw DocumentError.invalid }
    let url = URL(fileURLWithPath: CommandLine.arguments[1])
    let values = try url.resourceValues(forKeys: [.fileSizeKey, .isRegularFileKey])
    guard values.isRegularFile == true, let size = values.fileSize, size <= 16 * 1024 * 1024 else { throw DocumentError.tooLarge }
    let ext = url.pathExtension.lowercased()
    var text = ""
    var usedOCR = false
    if ext == "pdf" {
        guard let document = PDFDocument(url: url), document.pageCount <= 100 else { throw DocumentError.invalid }
        for index in 0..<document.pageCount {
            guard let page = document.page(at: index) else { throw DocumentError.invalid }
            let recognized: String = try autoreleasepool {
                if let original = page.string, !original.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { return original }
                let bounds = page.bounds(for: .mediaBox)
                let scale = min(2.0, 2400.0 / max(bounds.width, bounds.height))
                let image = page.thumbnail(of: NSSize(width: bounds.width * scale, height: bounds.height * scale), for: .mediaBox)
                guard let cg = image.cgImage(forProposedRect: nil, context: nil, hints: nil) else { throw DocumentError.invalid }
                usedOCR = true
                return try recognize(cg)
            }
            text += "\n[Page \(index + 1)]\n" + recognized
            if text.utf8.count > limit { throw DocumentError.tooLarge }
        }
    } else if ext == "doc" || ext == "rtf" {
        let content = try Data(contentsOf: url)
        let kind: NSAttributedString.DocumentType = ext == "rtf" ? .rtf : .docFormat
        text = try NSAttributedString(data: content, options: [.documentType: kind], documentAttributes: nil).string
    } else {
        guard let source = CGImageSourceCreateWithURL(url as CFURL, nil),
              let image = CGImageSourceCreateThumbnailAtIndex(source, 0, [kCGImageSourceCreateThumbnailFromImageAlways: true, kCGImageSourceThumbnailMaxPixelSize: 4096, kCGImageSourceCreateThumbnailWithTransform: true] as CFDictionary) else { throw DocumentError.invalid }
        text = try recognize(image)
        usedOCR = true
    }
    guard text.utf8.count <= limit, !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { throw DocumentError.invalid }
    let result: [String: Any] = ["text": text, "parser": "macOS Vision/PDFKit", "ocr": usedOCR]
    FileHandle.standardOutput.write(try JSONSerialization.data(withJSONObject: result))
} catch {
    FileHandle.standardError.write(Data("Document conversion failed\n".utf8))
    exit(1)
}
