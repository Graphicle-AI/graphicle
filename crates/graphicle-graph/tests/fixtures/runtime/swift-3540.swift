import Foundation

struct Rings { let move: Double = 0 }

func fetchRings() async -> Rings? { nil }

func load() async {
    async let pending = fetchRings()
    if let r = await pending {
        print(r.move)
    }
}

struct SentinelType { let marker = 1 }
