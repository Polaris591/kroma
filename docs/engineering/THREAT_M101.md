# M1-01 威胁探针与信任边界

脚本：`scripts/threat-probe.py`；机器输出：`evidence/m1-01/threat-probe.json`。

三项实测（本机 Linux 6.12.95，root）：

- **fd-inheritance → `launcher_controlled`**。显式传过去的描述符子进程能 `fstat` 成功；
  `close_fds=True` 时则 EBADF。所以"子进程看不到父进程的 fd"是**启动器**的行为
  （close_fds / CLOEXEC），不是内核边界。任何"我们的子进程隔离了句柄"的说法必须指明是谁关的。
- **namespaces → `enforced_by_bwrap_only`**。普通子进程与宿主共享 4/4 命名空间；
  bwrap 子进程在 pid、mnt、net 三项上与宿主不同（user 未改）。即隔离存在，但**只在用了 bwrap 时**。
- **peer-identity → 部分可用**。`SO_PEERCRED` 能拿到 pid/uid/gid，但同 uid 时内核无法区分
  敌意的同 uid 进程——broker 不能只靠 socket 路径私有来认身份，还需要 namespace 或凭据边界。

## 边界

这是探针，不是产品代码；它证明的是**这台机器现在能做到什么**，不是 Kroma 已有隔离。
第一版两个探针的结论是错的（把"启动器关了 fd"记成内核隔离；namespaces 的 status 与 detail
互相矛盾），已改成分别测"继承/不继承"和"普通子进程/bwrap 子进程"。

## 收口

无 Rust 改动，全量沿用 C00-12 收口时的 1060 passed 结果；证据见上述两个路径。
