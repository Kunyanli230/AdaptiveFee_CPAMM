# Adaptive-Fee CP-AMM

> A constant-product AMM (`x·y = k`) with **volatility-aware dynamic fees**, **EMA (oracle-less TWAP proxy)**, and a simple **circuit breaker**. Production-style Solidity (0.8.26), Remix-ready （two ERC-20 tokens required) .

> 中文简介： 一个在不依赖外部预言机的前提下，使用 **EMA（指数移动平均）** 近似 TWAP 的 **恒定乘积做市商**。手续费随**波动/滑点/深度**自适应调整，并内置**熔断阈值**。可直接在 Remix 运行 (需要先部署两个ERC-20）。

---

## Features | 特性

- **Adaptive Fees**: `fee = clamp(min + β·vol + γ·slip + δ·shallow)` (bps)  
- **Oracle-less TWAP**: **EMA** of on-chain spot; initialized on first liquidity  
- **Circuit Breaker**: reject swaps when relative deviation `|price−EMA|/EMA` exceeds a threshold  
- **Gas-aware**: minimal storage writes on hot paths; clear storage layout for subgraph/audit  
- **Drop-in**: Uniswap-style pool interface (`swap`, `addLiquidity`, `removeLiquidity`)


---

## Mechanics | 算法机制

**Constant product:** $x\cdot y = k,\quad 
\text{amountOut}=\dfrac{y\cdot \text{dx\_fee}}{x+\text{dx\_fee}}$

**EMA:** $\text{EMA}\leftarrow \text{EMA}+\alpha(\text{price}-\text{EMA}),\ \alpha\in(0,1]$

**Dynamic fee:**
$$
\text{fee}=\mathrm{clamp}\!\Big(\text{minFee}+\beta\cdot \text{vol}+\gamma\cdot \text{slip}+\delta\cdot \text{shallow},\;[\text{minFee},\text{maxFee}]\Big)
$$

- `vol = |price − EMA| / EMA`  
- `slip ≈ amountIn / (reserveIn + amountIn)`  
- `shallow = 1 − min(reserve0,reserve1)/(min(reserve0,reserve1)+K)`

---

## Parameters | 合约参数供更改

| Name | Type | Default | Meaning |
|---|---:|---:|---|
| `minFeeBps` | `uint256` | `30` | Floor fee (0.30%) |
| `maxFeeBps` | `uint256` | `120` | Cap fee (1.20%) |
| `betaVolBpsPer1e18` | `uint256` | `400` | Volatility coefficient (per 1.0) |
| `gammaSlipBpsPer1e18` | `uint256` | `300` | Slippage coefficient |
| `deltaShallowBpsPer1e18` | `uint256` | `200` | Shallow-depth coefficient |
| `emaAlpha` | `uint256` | `5e16` | EMA smoothing (0.05) |
| `breakerVolThreshold` | `uint256` | `2e17` | Circuit breaker (0.20) |


**Admin setters:** `setFeeBounds`, `setCoefficients`, `setEMAConfig`, `setBreaker`

---

