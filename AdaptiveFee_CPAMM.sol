// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

interface IERC20 {
    function totalSupply() external view returns (uint256);
    function balanceOf(address account) external view returns (uint256);
    function transfer(address recipient, uint256 amount)
        external
        returns (bool);
    function allowance(address owner, address spender)
        external
        view
        returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
    function transferFrom(address sender, address recipient, uint256 amount)
        external
        returns (bool);
}

contract AdaptiveCPAMM {
    // -------- Tokens --------
    IERC20 public immutable token0;
    IERC20 public immutable token1;

    // -------- Reserves --------
    uint256 public reserve0;
    uint256 public reserve1;

    // -------- LP shares --------
    uint256 public totalSupply;
    mapping(address => uint256) public balanceOf;

    // -------- Owner (for parameter tuning) --------
    address public owner;

    // -------- Dynamic fee parameters (bps = 1e4) --------
    // fee = clamp( minFee + beta*vol + gamma*slip + delta*shallow , [minFee, maxFee] )
    uint256 public minFeeBps = 30;     // 0.30%
    uint256 public maxFeeBps = 120;    // 1.20%
    uint256 public betaVolBpsPer1e18   = 400;  // β: +4.00% per 1.0 of volatility (example, tunable)
    uint256 public gammaSlipBpsPer1e18 = 300;  // γ: +3.00% per 1.0 of slippage
    uint256 public deltaShallowBpsPer1e18 = 200; // δ: +2.00% per 1.0 of depth shalowness

    // Circuit breaker: reject swaps if |price-ema|/ema > threshold
    uint256 public breakerVolThreshold = 2e17; // 0.20 = 20%

    // -------- EMA (oracle-less TWAP proxy) --------
    // emaPrice = EMA(spotPrice), scale 1e18
    // spot price = reserve1 / reserve0 (price of token0 in token1)
    uint256 public emaPrice;          // 1e18 scale
    uint256 public emaAlpha = 5e16;   // 0.05 smoothing per update (range (0,1e18])
    uint256 public lastEmaUpdate;     // last update timestamp

    // -------- Events --------
    event Swap(address indexed trader, address indexed tokenIn, uint256 amountIn, address indexed tokenOut, uint256 amountOut, uint256 feeBps);
    event Mint(address indexed sender, uint256 amount0, uint256 amount1, uint256 shares);
    event Burn(address indexed sender, uint256 shares, uint256 amount0, uint256 amount1);
    event ParamsUpdated();
    event CircuitBreakerTriggered(address indexed trader, uint256 vol);

    // -------- Modifiers --------
    modifier onlyOwner() {
        require(msg.sender == owner, "not owner");
        _;
    }

    constructor(address _token0, address _token1) {
        require(_token0 != _token1, "identical");
        token0 = IERC20(_token0);
        token1 = IERC20(_token1);
        owner = msg.sender;
    }

    // -------------------- Admin: parameter tuning --------------------
    function setFeeBounds(uint256 _minBps, uint256 _maxBps) external onlyOwner {
        // example cap: max <= 2%
        require(_minBps <= _maxBps && _maxBps <= 2000, "bad bounds");
        minFeeBps = _minBps;
        maxFeeBps = _maxBps;
        emit ParamsUpdated();
    }

    function setCoefficients(
        uint256 _betaVolBpsPer1e18,
        uint256 _gammaSlipBpsPer1e18,
        uint256 _deltaShallowBpsPer1e18
    ) external onlyOwner {
        betaVolBpsPer1e18 = _betaVolBpsPer1e18;
        gammaSlipBpsPer1e18 = _gammaSlipBpsPer1e18;
        deltaShallowBpsPer1e18 = _deltaShallowBpsPer1e18;
        emit ParamsUpdated();
    }

    function setEMAConfig(uint256 _alpha) external onlyOwner {
        require(_alpha > 0 && _alpha <= 1e18, "alpha out of range");
        emaAlpha = _alpha;
        emit ParamsUpdated();
    }

    function setBreaker(uint256 _volThreshold) external onlyOwner {
        breakerVolThreshold = _volThreshold; // e.g., 0.2e18 = 20%
        emit ParamsUpdated();
    }

    // -------------------- Internal math --------------------
    function _min(uint256 x, uint256 y) private pure returns (uint256) {
        return x <= y ? x : y;
    }

    function _sqrt(uint256 y) private pure returns (uint256 z) {
        // Integer sqrt via Babylonian method
        if (y > 3) {
            z = y;
            uint256 x = y / 2 + 1;
            while (x < z) {
                z = x;
                x = (y / x + x) / 2;
            }
        } else if (y != 0) {
            z = 1;
        }
    }

    function _update(uint256 _reserve0, uint256 _reserve1) private {
        reserve0 = _reserve0;
        reserve1 = _reserve1;
    }

    function _mint(address _to, uint256 _amount) private {
        balanceOf[_to] += _amount;
        totalSupply += _amount;
    }

    function _burn(address _from, uint256 _amount) private {
        balanceOf[_from] -= _amount;
        totalSupply -= _amount;
    }

    // Current spot price: token0 priced in token1 (1e18 scale)
    function _spotPrice1e18(uint256 x, uint256 y) internal pure returns (uint256) {
        require(x > 0 && y > 0, "bad reserves");
        return (y * 1e18) / x;
    }

    // Update EMA per trade (initialize with spot on first call)
    function _updateEMA(uint256 price1e18) internal {
        if (emaPrice == 0) {
            emaPrice = price1e18;
        } else {
            uint256 ema = emaPrice;
            if (price1e18 >= ema) {
                uint256 diff = price1e18 - ema;
                emaPrice = ema + (diff * emaAlpha) / 1e18;
            } else {
                uint256 diff = ema - price1e18;
                emaPrice = ema - (diff * emaAlpha) / 1e18;
            }
        }
        lastEmaUpdate = block.timestamp;
    }

    // Compute dynamic fee and its components (for front-end/Remix inspection)
    function _computeDynamicFeeBps(
        bool isToken0In,
        uint256 amountIn
    ) internal view returns (
        uint256 feeBps,
        uint256 vol1e18,
        uint256 slip1e18,
        uint256 shallow1e18
    ) {
        require(amountIn > 0, "amountIn=0");

        (uint256 rin, uint256 rout) = isToken0In
            ? (reserve0, reserve1)
            : (reserve1, reserve0);

        require(rin > 0 && rout > 0, "no liquidity");

        // --- volatility proxy ---
        uint256 priceNow = _spotPrice1e18(reserve0, reserve1);
        if (emaPrice == 0) {
            vol1e18 = 0; // before EMA init, no volatility surcharge
        } else {
            if (priceNow >= emaPrice) {
                vol1e18 = ((priceNow - emaPrice) * 1e18) / emaPrice;
            } else {
                vol1e18 = ((emaPrice - priceNow) * 1e18) / emaPrice;
            }
        }

        // --- slippage proxy (simple) ---
        // slip ≈ amountIn / (rin + amountIn), in (0,1)
        slip1e18 = (amountIn * 1e18) / (rin + amountIn);

        // --- shallow-depth proxy ---
        // shallow = 1 - minRes/(minRes + K); smaller K -> more sensitivity
        uint256 minRes = reserve0 < reserve1 ? reserve0 : reserve1;
        uint256 K = 1000 ether; // depth scale factor (example)
        shallow1e18 = 1e18 - ((minRes * 1e18) / (minRes + K));

        // linear combination + clamping
        uint256 dynamicPart =
            (betaVolBpsPer1e18 * vol1e18) / 1e18 +
            (gammaSlipBpsPer1e18 * slip1e18) / 1e18 +
            (deltaShallowBpsPer1e18 * shallow1e18) / 1e18;

        uint256 raw = minFeeBps + dynamicPart;
        if (raw < minFeeBps) raw = minFeeBps;
        if (raw > maxFeeBps) raw = maxFeeBps;

        feeBps = raw;
    }

    // Public view helper: preview the dynamic fee for a given trade
    function getDynamicFee(address _tokenIn, uint256 _amountIn)
        external
        view
        returns (uint256 feeBps, uint256 vol1e18, uint256 slip1e18, uint256 shallow1e18)
    {
        require(_tokenIn == address(token0) || _tokenIn == address(token1), "invalid token");
        bool isToken0In = (_tokenIn == address(token0));
        (feeBps, vol1e18, slip1e18, shallow1e18) = _computeDynamicFeeBps(isToken0In, _amountIn);
    }

    // Public view helper: state snapshot (price/EMA/reserves)
    function getState()
        external
        view
        returns (uint256 px1e18, uint256 ema1e18, uint256 r0, uint256 r1, uint256 ts)
    {
        px1e18 = (reserve0 > 0 && reserve1 > 0) ? _spotPrice1e18(reserve0, reserve1) : 0;
        ema1e18 = emaPrice;
        r0 = reserve0; r1 = reserve1; ts = lastEmaUpdate;
    }

    // -------------------- Core: swap --------------------
    function swap(address _tokenIn, uint256 _amountIn)
        external
        returns (uint256 amountOut, uint256 feeBps)
    {
        require(
            _tokenIn == address(token0) || _tokenIn == address(token1),
            "invalid token"
        );
        require(_amountIn > 0, "amount in = 0");

        bool isToken0 = _tokenIn == address(token0);
        (IERC20 tokenIn, IERC20 tokenOut, uint256 reserveIn, uint256 reserveOut)
            = isToken0 ? (token0, token1, reserve0, reserve1) : (token1, token0, reserve1, reserve0);

        // Dynamic fee (destructure safely)
        uint256 vol1e18;
        (feeBps, vol1e18, , ) = _computeDynamicFeeBps(isToken0, _amountIn);

        // Circuit breaker on excessive volatility
        if (vol1e18 > breakerVolThreshold) {
            emit CircuitBreakerTriggered(msg.sender, vol1e18);
            revert("vol too high");
        }

        // Pull tokenIn
        require(tokenIn.transferFrom(msg.sender, address(this), _amountIn), "transferFrom failed");

        // Constant product swap with fee applied to amountIn
        uint256 amountInWithFee = (_amountIn * (10000 - feeBps)) / 10000;
        amountOut = (reserveOut * amountInWithFee) / (reserveIn + amountInWithFee);
        require(amountOut > 0, "amountOut=0");

        // Send tokenOut
        require(tokenOut.transfer(msg.sender, amountOut), "transfer failed");

        // Update reserves
        _update(token0.balanceOf(address(this)), token1.balanceOf(address(this)));

        // Update EMA using the latest spot
        uint256 priceNow = _spotPrice1e18(reserve0, reserve1);
        _updateEMA(priceNow);

        emit Swap(msg.sender, _tokenIn, _amountIn, address(tokenOut), amountOut, feeBps);
    }

    // -------------------- Liquidity --------------------
    function addLiquidity(uint256 _amount0, uint256 _amount1)
        external
        returns (uint256 shares)
    {
        // Pull tokens from provider
        require(token0.transferFrom(msg.sender, address(this), _amount0), "t0 transferFrom failed");
        require(token1.transferFrom(msg.sender, address(this), _amount1), "t1 transferFrom failed");

        // Enforce price invariance on add (same ratio)
        if (reserve0 > 0 || reserve1 > 0) {
            require(reserve0 * _amount1 == reserve1 * _amount0, "x / y != dx / dy");
        }

        // Mint shares proportionally to liquidity increase
        if (totalSupply == 0) {
            shares = _sqrt(_amount0 * _amount1);
            // Initialize EMA on first liquidity
            if (shares > 0 && emaPrice == 0 && (_amount0 > 0 && _amount1 > 0)) {
                uint256 p0 = _spotPrice1e18(_amount0, _amount1);
                emaPrice = p0;
                lastEmaUpdate = block.timestamp;
            }
        } else {
            shares = _min(
                (_amount0 * totalSupply) / reserve0,
                (_amount1 * totalSupply) / reserve1
            );
        }
        require(shares > 0, "shares = 0");

        _mint(msg.sender, shares);
        _update(token0.balanceOf(address(this)), token1.balanceOf(address(this)));

        // Optionally update EMA after add
        if (reserve0 > 0 && reserve1 > 0) {
            _updateEMA(_spotPrice1e18(reserve0, reserve1));
        }

        emit Mint(msg.sender, _amount0, _amount1, shares);
    }

    function removeLiquidity(uint256 _shares)
        external
        returns (uint256 amount0, uint256 amount1)
    {
        // Snapshot balances (>= reserves)
        uint256 bal0 = token0.balanceOf(address(this));
        uint256 bal1 = token1.balanceOf(address(this));

        // Pro-rata redemption
        amount0 = (_shares * bal0) / totalSupply;
        amount1 = (_shares * bal1) / totalSupply;
        require(amount0 > 0 && amount1 > 0, "amount0 or amount1 = 0");

        _burn(msg.sender, _shares);
        _update(bal0 - amount0, bal1 - amount1);

        require(token0.transfer(msg.sender, amount0), "t0 transfer failed");
        require(token1.transfer(msg.sender, amount1), "t1 transfer failed");

        // Optionally update EMA after remove
        if (reserve0 > 0 && reserve1 > 0) {
            _updateEMA(_spotPrice1e18(reserve0, reserve1));
        }

        emit Burn(msg.sender, _shares, amount0, amount1);
    }
}
