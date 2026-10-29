// Native services for the Mac App Store build, called from Rust (src/store.rs).
//
//   * StoreKit 2: NavDB Studio Pro subscriptions
//   * security-scoped bookmarks: keep access to files the user picked
//     (SQLite/DuckDB databases, certificates, export folders) across launches
//
// Strings cross the boundary as NUL-terminated UTF-8; strings returned to Rust
// are malloc'ed and released with navdb_free.

import Foundation
import StoreKit

/// Product identifiers configured in App Store Connect.
let productIDs = ["dev.navdb.studio.pro.monthly", "dev.navdb.studio.pro.yearly"]

final class StoreState: @unchecked Sendable {
    static let shared = StoreState()
    private let lock = NSLock()
    private var _products: [Product] = []
    private var _pro = false
    private var _busy = false
    private var _message = ""
    private var _trialEligible = false

    func with<T>(_ f: (StoreState) -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return f(self)
    }

    var products: [Product] { get { with { $0._products } } set { with { $0._products = newValue } } }
    var pro: Bool { get { with { $0._pro } } set { with { $0._pro = newValue } } }
    var busy: Bool { get { with { $0._busy } } set { with { $0._busy = newValue } } }
    var message: String { get { with { $0._message } } set { with { $0._message = newValue } } }
    var trialEligible: Bool { get { with { $0._trialEligible } } set { with { $0._trialEligible = newValue } } }
}

private func refreshEntitlements() async {
    var active = false
    for await result in Transaction.currentEntitlements {
        if case .verified(let t) = result, productIDs.contains(t.productID), t.revocationDate == nil {
            if let exp = t.expirationDate, exp < Date() { continue }
            active = true
        }
    }
    StoreState.shared.pro = active
    await refreshTrialEligibility()
}

/// The free trial is offered once per subscription group; only show it to
/// people who can still get it.
private func refreshTrialEligibility() async {
    guard let sub = StoreState.shared.products.first?.subscription else { return }
    StoreState.shared.trialEligible = await sub.isEligibleForIntroOffer
}

private func loadProducts() async {
    do {
        let list = try await Product.products(for: productIDs)
        StoreState.shared.products = list.sorted { $0.price < $1.price }
        await refreshTrialEligibility()
        if list.isEmpty {
            StoreState.shared.message = "Subscriptions are not available right now. Please try again later."
        }
    } catch {
        StoreState.shared.message = "Could not load subscriptions: \(error.localizedDescription)"
    }
}

private func cString(_ s: String) -> UnsafeMutablePointer<CChar>? {
    strdup(s)
}

private func jsonString(_ v: Any) -> String {
    guard let d = try? JSONSerialization.data(withJSONObject: v), let s = String(data: d, encoding: .utf8) else { return "null" }
    return s
}

@_cdecl("navdb_free")
public func navdb_free(_ p: UnsafeMutablePointer<CChar>?) {
    free(p)
}

/// Load products, read current entitlements and listen for renewals/refunds.
@_cdecl("navdb_store_start")
public func navdb_store_start() {
    Task.detached {
        await loadProducts()
        await refreshEntitlements()
        for await update in Transaction.updates {
            if case .verified(let t) = update {
                await t.finish()
            }
            await refreshEntitlements()
        }
    }
}

/// Ask the App Store for the subscriptions again, e.g. after a network error.
@_cdecl("navdb_store_reload")
public func navdb_store_reload() {
    StoreState.shared.message = ""
    Task.detached {
        await loadProducts()
    }
}

@_cdecl("navdb_store_is_pro")
public func navdb_store_is_pro() -> Bool {
    StoreState.shared.pro
}

/// JSON: {"pro":bool,"busy":bool,"message":str,"products":[{id,name,description,price,period,trial}]}
@_cdecl("navdb_store_status")
public func navdb_store_status() -> UnsafeMutablePointer<CChar>? {
    let s = StoreState.shared
    let products: [[String: Any]] = s.products.map { p in
        var period = ""
        var trial = ""
        if let sub = p.subscription {
            period = describe(sub.subscriptionPeriod)
            if s.trialEligible, let offer = sub.introductoryOffer, offer.paymentMode == .freeTrial {
                trial = describe(offer.period)
            }
        }
        return ["id": p.id, "name": p.displayName, "description": p.description, "price": p.displayPrice, "period": period, "trial": trial]
    }
    return cString(jsonString(["pro": s.pro, "busy": s.busy, "message": s.message, "products": products]))
}

private func describe(_ p: Product.SubscriptionPeriod) -> String {
    let unit: String
    switch p.unit {
    case .day: unit = "day"
    case .week: unit = "week"
    case .month: unit = "month"
    case .year: unit = "year"
    @unknown default: unit = "period"
    }
    return p.value == 1 ? unit : "\(p.value) \(unit)s"
}

@_cdecl("navdb_store_purchase")
public func navdb_store_purchase(_ id: UnsafePointer<CChar>) {
    let pid = String(cString: id)
    let s = StoreState.shared
    if s.busy { return }
    s.busy = true
    s.message = ""
    Task.detached {
        defer { StoreState.shared.busy = false }
        if StoreState.shared.products.isEmpty { await loadProducts() }
        guard let product = StoreState.shared.products.first(where: { $0.id == pid }) else {
            StoreState.shared.message = "Subscription is not available right now."
            return
        }
        do {
            switch try await product.purchase() {
            case .success(let result):
                if case .verified(let t) = result {
                    await t.finish()
                } else {
                    StoreState.shared.message = "The purchase could not be verified."
                }
                await refreshEntitlements()
            case .pending:
                StoreState.shared.message = "The purchase is pending approval."
            case .userCancelled:
                break
            @unknown default:
                break
            }
        } catch {
            StoreState.shared.message = error.localizedDescription
        }
    }
}

@_cdecl("navdb_store_restore")
public func navdb_store_restore() {
    let s = StoreState.shared
    if s.busy { return }
    s.busy = true
    s.message = ""
    Task.detached {
        defer { StoreState.shared.busy = false }
        do {
            try await AppStore.sync()
        } catch {
            StoreState.shared.message = error.localizedDescription
        }
        await refreshEntitlements()
        if !StoreState.shared.pro && StoreState.shared.message.isEmpty {
            StoreState.shared.message = "No active subscription was found for this Apple Account."
        }
    }
}

// MARK: - security-scoped bookmarks

/// Base64 app-scoped bookmark for `path`, or NULL when it cannot be created.
@_cdecl("navdb_bookmark_create")
public func navdb_bookmark_create(_ path: UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar>? {
    let url = URL(fileURLWithPath: String(cString: path))
    guard let data = try? url.bookmarkData(options: [.withSecurityScope], includingResourceValuesForKeys: nil, relativeTo: nil) else { return nil }
    return cString(data.base64EncodedString())
}

/// Resolve a bookmark and start accessing it. Returns the current path of the
/// item (it may have moved), or NULL when the bookmark is no longer valid.
@_cdecl("navdb_bookmark_open")
public func navdb_bookmark_open(_ b64: UnsafePointer<CChar>) -> UnsafeMutablePointer<CChar>? {
    guard let data = Data(base64Encoded: String(cString: b64)) else { return nil }
    var stale = false
    guard let url = try? URL(resolvingBookmarkData: data, options: [.withSecurityScope], relativeTo: nil, bookmarkDataIsStale: &stale) else { return nil }
    guard url.startAccessingSecurityScopedResource() else { return nil }
    return cString(url.path)
}
