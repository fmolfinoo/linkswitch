mod config;
mod elevate;
mod log;
mod net;

fn main() {
    let snap = net::Snapshot::read();
    let (eth, wifi) = snap.candidates();

    println!("== candidates ==");
    for n in eth.iter().chain(wifi.iter()) {
        println!(
            "  [{:?}/{:?}] if{} {:<28} up={} media={} {} {:?}",
            n.kind,
            n.tier,
            n.if_index,
            n.label(),
            n.oper_up,
            n.media_connected,
            n.link_speed_text().unwrap_or_else(|| "-".into()),
            n.ipv4,
        );
    }

    println!("== all adapters ==");
    for n in &snap.nics {
        println!(
            "  if{:<4} type={:<4} {:?}/{:?} {}",
            n.if_index,
            n.if_type,
            n.kind,
            n.tier,
            n.label()
        );
    }

    println!("== default routes ==");
    for r in &snap.routes {
        println!(
            "  if{:<4} v6={} route={} iface={} total={}",
            r.if_index, r.family_is_v6, r.route_metric, r.iface_metric, r.total
        );
    }

    let e = eth.first().map(|n| n.luid);
    let w = wifi.first().map(|n| n.luid);
    println!("== verdict == {:?}", snap.verdict(e, w));
    println!("== wifi == {:?}", net::wifi::status());
}
