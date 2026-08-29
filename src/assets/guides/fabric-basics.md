# Fabric 服务端工作原理速览

Fabric 是轻量 mod 加载器。Fabric 服务端的本质：

1. 官方 meta 提供"bundle jar"——一个自解压引导包，首次启动时自动下载
   Minecraft 官方服务端与 Fabric loader 库文件到当前目录；
2. 因此首次启动耗时明显更长（要拉取库文件），日志出现 Done 才算就绪；
3. mod（.jar 文件）放进服务端目录下的 `mods/` 子目录即可生效；
4. mod 必须同时满足：MC 版本匹配 + loader 版本兼容 + 依赖齐备。
   缺依赖时启动会报 `ModResolutionException`；
5. Fabric 官方服务端 bundle 不附带哈希元数据，本系统通过"仅使用官方 meta 域名 +
   HTTPS"约束来源安全，下载后的文件哈希记录在案。

与 Paper 的取舍：Paper 面向插件（ Bukkit 生态），混合认证插件成熟；
Fabric 面向 mod（玩家玩法扩展）。两者不可混用插件与 mod。
