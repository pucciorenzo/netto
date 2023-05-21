use std::time::{Duration, Instant};
use actix::{Actor, Context, AsyncContext, Addr};
use anyhow::anyhow;
use libbpf_rs::MapFlags;
use powercap::{IntelRapl, PowerCap};
use tokio::sync::mpsc::Sender;
use crate::{ksyms::{Counts, KSyms}, common::{event_types_EVENT_MAX, self, event_types_EVENT_SOCK_SENDMSG, event_types_EVENT_NET_TX_SOFTIRQ, event_types_EVENT_NET_RX_SOFTIRQ}, bpf::ProgSkel};
use libc::{mmap, PROT_READ, MAP_SHARED};
use super::{metrics_collector::MetricsCollector, MetricUpdate, SubmitUpdate};

/// Actor responsible for interacting with BPF via shared maps,
/// retrieve stack traces from the ring buffer, and analyze them
/// to provide user-facing performance metrics.
pub struct TraceAnalyzer {
    /// libbpf's skeleton
    skel: ProgSkel<'static>,

    /// Pointer to the mmaped stack traces array map
    stack_traces_ptr: *const u64,

    /// Vec of one Counts for each CPU
    counts: Vec<Counts>,

    /// Kernel symbols for processing the traces
    ksyms: KSyms,
    
    /// Link to the open powercap interface for power queries
    rapl: IntelRapl,

    /// Addr of the `MetricsCollector` actor
    metrics_collector_addr: Addr<MetricsCollector>,
    
    /// Interface for sending unrecoverable runtime errors to the
    /// main task, triggering the program termination
    error_catcher_sender: Sender<anyhow::Error>,

    // State-keeping fields

    /// Timestamp of the previous update cycle.
    /// Useful to calculate the delta-time.
    prev_update_ts: Instant,

    /// Total times up to the previous update cycle,
    /// for each cpu for each event
    prev_total_times: Vec<Vec<u64>>,

    /// Total energy, as reported by RAPL, up to the
    /// previous update cycle
    prev_total_energy: u64,
}

impl TraceAnalyzer {
    /// Build a new TraceAnalyzer instance.
    /// 
    /// Note that the `per_cpu` map is passed by its id in order
    /// to be able to acquire it as an owned `libbpf_rs::Map` and
    /// avoid the reference to the lifetime of the main skel.
    pub fn new(
        skel: ProgSkel<'static>,
        num_possible_cpus: usize,
        metrics_collector_addr: Addr<MetricsCollector>,
        error_catcher_sender: Sender<anyhow::Error>
    ) -> anyhow::Result<Self> {
        let stack_traces_ptr = unsafe { mmap(
            std::ptr::null_mut(),
            std::mem::size_of::<u64>() * 128 * 20_000,
            PROT_READ,
            MAP_SHARED,
            skel.maps().stack_traces().fd(),
            0
        ) } as *const u64;

        let rapl = PowerCap::try_default()
            .map_err(|e| anyhow!("Failed to acquire powercap interface: {e:?}"))?
            .intel_rapl;

        Ok(Self {
            skel,
            stack_traces_ptr,
            counts: vec![Counts::default(); num_possible_cpus],
            ksyms: KSyms::load()?,
            rapl,
            metrics_collector_addr,
            error_catcher_sender,
            prev_update_ts: Instant::now(),
            prev_total_times: vec![vec![0;  event_types_EVENT_MAX as _]; num_possible_cpus],
            prev_total_energy: 0
        })
    }

    /// Main user-space update loop
    #[inline]
    fn run_interval(&mut self) -> anyhow::Result<()> {
        let now = Instant::now();
        
        // Update state
        let delta_time = {
            let dt = now.duration_since(self.prev_update_ts);
            self.prev_update_ts = now;
            dt
        };
        let delta_energy = {
            let current_total_energy = self.rapl
                .sockets
                .values()
                .flat_map(|socket| socket.energy())
                .sum();
            let delta_energy = current_total_energy - self.prev_total_energy;
            self.prev_total_energy = current_total_energy;
            delta_energy
        };
        
        // Reset counts to zero
        for counts in &mut self.counts {
            *counts = Counts::default();
        }

        // Drain the stack traces array
        {
            // Swap buffer slots and get the number of stack traces in the previously active slot
            let slot_off = self.skel.bss().stack_traces_slot_off as usize;
            let num_traces_ref;
            (self.skel.bss().stack_traces_slot_off, num_traces_ref) = if slot_off > 0 {
                (0,      &mut self.skel.bss().stack_traces_count_slot_1)
            } else {
                (10_000, &mut self.skel.bss().stack_traces_count_slot_0)
            };

            // Make sure to read the count *after* swapping the slots
            let num_traces = *num_traces_ref;
            
            // Count symbols
            unsafe {
                for trace_ptr in (0..num_traces as usize).map(|trace_idx| self.stack_traces_ptr.add((slot_off + trace_idx) * 128)) {
                    // Get the cpuid
                    let (trace_size, cpuid) = {
                        let v = trace_ptr.read_volatile();

                        // Note that the trace size is encoded in bytes in the map, bu we care about number of u64s
                        (v >> 35, v & 0xFFFFFFFF)
                    };

                    self.counts[cpuid as usize].acc_trace(
                        &self.ksyms,
                        trace_ptr.add(1),
                        trace_size as _
                    );
                }
            }

            // Reset the stack traces index for this slot
            *num_traces_ref = 0;
        }

        // Get a reference to the counts
        let counts = &self.counts;

        // Lookup in the per-cpu map
        let stats = self.skel.maps().per_cpu()
            .lookup_percpu(&0i32.to_le_bytes(), MapFlags::empty())?
            .ok_or(anyhow!("Unexpected None returned for lookup into the \"per_cpu\" map"))?;
        
        let total_cpu_frac = stats
            .iter()
            .zip(self.prev_total_times.iter_mut())
            .enumerate()
            .map(|(cpuid, (cpu_stats, prev_total_cpu_times))| {
                unsafe {
                    // Read the data as unaligned because we do not have any alignment guarantees at this point
                    (cpu_stats.as_ptr() as *const common::per_cpu_data).read_unaligned()
                }.events
                    .iter()
                    .zip(prev_total_cpu_times.iter_mut())
                    .enumerate()
                    .map(|(event_idx, (common::per_event_data { total_time, .. }, prev_total_time))| {
                        let delta_cpu_time = total_time - *prev_total_time;
                        *prev_total_time = *total_time;
                        let cpu_frac = (delta_cpu_time as f64) / (delta_time.as_nanos() as f64);

                        #[allow(non_upper_case_globals)]
                        let metric_name = match event_idx as u32 {
                            event_types_EVENT_SOCK_SENDMSG => "TX syscalls",
                            event_types_EVENT_NET_TX_SOFTIRQ => "TX softirq",
                            event_types_EVENT_NET_RX_SOFTIRQ => {
                                // Update sub-events
                                let denominator = counts[cpuid].net_rx_action.max(1) as f64;
                                
                                // Driver poll
                                self.metrics_collector_addr.do_send(MetricUpdate {
                                    name: "RX softirq/Driver poll",
                                    cpuid,
                                    cpu_frac: cpu_frac * (counts[cpuid].__napi_poll - counts[cpuid].netif_receive_skb) as f64 / denominator
                                });

                                // XDP generic
                                self.metrics_collector_addr.do_send(MetricUpdate {
                                    name: "RX softirq/XDP generic",
                                    cpuid,
                                    cpu_frac: cpu_frac * counts[cpuid].do_xdp_generic as f64 / denominator
                                });

                                // TC classify
                                self.metrics_collector_addr.do_send(MetricUpdate {
                                    name: "RX softirq/TC classify",
                                    cpuid,
                                    cpu_frac: cpu_frac * counts[cpuid].tcf_classify as f64 / denominator
                                });

                                // NF ingress
                                self.metrics_collector_addr.do_send(MetricUpdate {
                                    name: "RX softirq/NF ingress",
                                    cpuid,
                                    cpu_frac: cpu_frac * counts[cpuid].nf_netdev_ingress as f64 / denominator
                                });

                                // Bridging
                                self.metrics_collector_addr.do_send(MetricUpdate {
                                    name: "RX softirq/Bridging",
                                    cpuid,
                                    cpu_frac: cpu_frac * (counts[cpuid].br_handle_frame - counts[cpuid].netif_receive_skb_sub_br) as f64 / denominator
                                });

                                // NF prerouting
                                self.metrics_collector_addr.do_send(MetricUpdate {
                                    name: "RX softirq/NF prerouting/v4",
                                    cpuid,
                                    cpu_frac: cpu_frac * counts[cpuid].nf_prerouting_v4 as f64 / denominator
                                });

                                self.metrics_collector_addr.do_send(MetricUpdate {
                                    name: "RX softirq/NF prerouting/v6",
                                    cpuid,
                                    cpu_frac: cpu_frac * counts[cpuid].nf_prerouting_v6 as f64 / denominator
                                });

                                // Forwarding
                                self.metrics_collector_addr.do_send(MetricUpdate {
                                    name: "RX softirq/Forwarding/v4",
                                    cpuid,
                                    cpu_frac: cpu_frac * counts[cpuid].ip_forward as f64 / denominator
                                });

                                self.metrics_collector_addr.do_send(MetricUpdate {
                                    name: "RX softirq/Forwarding/v6",
                                    cpuid,
                                    cpu_frac: cpu_frac * counts[cpuid].ip6_forward as f64 / denominator
                                });

                                // Local deliver
                                self.metrics_collector_addr.do_send(MetricUpdate {
                                    name: "RX softirq/Local delivery/v4",
                                    cpuid,
                                    cpu_frac: cpu_frac * counts[cpuid].ip_local_deliver as f64 / denominator
                                });

                                self.metrics_collector_addr.do_send(MetricUpdate {
                                    name: "RX softirq/Local delivery/v6",
                                    cpuid,
                                    cpu_frac: cpu_frac * counts[cpuid].ip6_input as f64 / denominator
                                });
                                
                                "RX softirq"
                            },
                            _ => unreachable!()
                        };

                        self.metrics_collector_addr.do_send(MetricUpdate {
                            name: metric_name,
                            cpuid,
                            cpu_frac
                        });

                        cpu_frac
                    })
                    .sum::<f64>()
            })
            .sum::<f64>() / (self.prev_total_times.len() as f64);

        self.metrics_collector_addr.do_send(SubmitUpdate {
            net_power_w: ((delta_energy as f64) * total_cpu_frac) / (delta_time.as_secs_f64() * 1_000_000.0),
            user_space_overhead: now.elapsed().as_secs_f64() / delta_time.as_secs_f64()
        });

        Ok(())
    }
}

impl Actor for TraceAnalyzer {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        ctx.run_interval(Duration::from_millis(500), |act, _| {
            if let Err(e) = act.run_interval() {
                act.error_catcher_sender.blocking_send(e).unwrap();
            }
        });
    }
}